// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

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
                setup = SetupModel(app: app, mode: .firstRun, deviceName: Host.current().localizedName ?? "Mac")
            }
        }
    }
}

struct SetupSteps: View {
    @Bindable var setup: SetupModel
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if setup.step != .done {
                StepIndicator(step: setup.step)
            }
            switch setup.step {
            case .server: ServerStep(setup: setup)
            case .signIn: SignInStep(setup: setup)
            case .folder: FolderStep(setup: setup)
            case .done: DoneStep { dismissWindow(id: WindowID.setup) }
            }
        }
        .padding(24)
        .disabled(setup.isBusy)
    }
}

private struct StepIndicator: View {
    let step: SetupModel.Step

    var body: some View {
        HStack(spacing: 16) {
            item(1, "Server", for: .server)
            item(2, "Sign in", for: .signIn)
            item(3, "Folder", for: .folder)
        }
        .font(.callout)
    }

    private func item(_ number: Int, _ title: LocalizedStringKey, for target: SetupModel.Step) -> some View {
        let reached = target.order <= step.order
        return HStack(spacing: 6) {
            Text(verbatim: number.formatted())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(reached ? Color.onKoi : .secondary)
                .frame(width: 16, height: 16)
                .background(Circle().fill(reached ? Color.koi : Color.primary.opacity(0.1)))
            Text(title)
                .fontWeight(step == target ? .semibold : .regular)
                .foregroundStyle(step == target ? .primary : .secondary)
        }
    }
}

extension SetupModel.Step {
    fileprivate var order: Int {
        switch self {
        case .server: 0
        case .signIn: 1
        case .folder: 2
        case .done: 3
        }
    }
}

private struct StepHeader: View {
    let systemImage: String
    let title: LocalizedStringKey
    let help: String

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: systemImage)
                .font(.system(size: 22))
                .foregroundStyle(Color.koi)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.system(size: 17, weight: .semibold))
                Text(help)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct Field<Accessory: View>: View {
    let label: LocalizedStringKey
    @Binding var text: String
    var monospaced = false
    var prompt: Text?
    @ViewBuilder var accessory: Accessory

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label)
                .foregroundStyle(.secondary)
            TextField(label, text: $text, prompt: prompt)
                .labelsHidden()
                .textFieldStyle(.roundedBorder)
                .font(monospaced ? .body.monospaced() : .body)
            accessory
        }
    }
}

extension Field where Accessory == EmptyView {
    init(label: LocalizedStringKey, text: Binding<String>, monospaced: Bool = false, prompt: Text? = nil) {
        self.init(label: label, text: text, monospaced: monospaced, prompt: prompt) { EmptyView() }
    }
}

private struct ServerStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            StepHeader(
                systemImage: "server.rack",
                title: "Connect to your server",
                help: String(localized: "Enter your server's address, such as https://koidrive.alice.workers.dev.")
            )
            Field(label: "Server address", text: $setup.serverURL, prompt: Text(verbatim: "https://"))
                .onSubmit(connect)
            if setup.asksForUsageReports {
                Toggle(isOn: $setup.sendsUsageReports) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Send anonymous usage reports")
                        Text("It never shares file names, paths or contents.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .toggleStyle(.checkbox)
                .tint(.koi)
            }
            if let error = setup.error {
                ErrorBanner(message: error.message)
            }
            HStack {
                Spacer()
                Button("Connect", action: connect)
                    .buttonStyle(.prominent)
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
            if setup.claimsServer {
                StepHeader(
                    systemImage: "key",
                    title: "Claim this server",
                    help: String(localized: "This server has no owner yet. In the Cloudflare dashboard, open the Worker's Observability and search \"Koidrive claim code\".")
                )
            } else {
                StepHeader(
                    systemImage: "laptopcomputer",
                    title: "Sign in",
                    help: String(localized: "On a signed-in device, choose Settings > Account > Add a Device to get a code.")
                )
            }
            if let server = setup.server {
                HStack {
                    Image(systemName: "server.rack")
                        .foregroundStyle(.secondary)
                    Text(verbatim: server.url)
                        .font(.body.monospaced())
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer()
                    Text(verbatim: "v" + server.serverVersion)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .background(RoundedRectangle(cornerRadius: 8).fill(.quaternary.opacity(0.5)))
                .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
            }
            Group {
                if setup.claimsServer {
                    Field(label: "Your name", text: $setup.userName)
                    Field(label: "Claim code", text: $setup.code, monospaced: true)
                } else {
                    Field(label: "Pairing code", text: $setup.code, monospaced: true) {
                        Text("Codes expire after a few minutes.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Field(label: "Name of this Mac", text: $setup.deviceName)
            }
            .onSubmit(signIn)
            if let error = setup.error {
                ErrorBanner(message: error.message)
            }
            HStack {
                Button("Change Server") { setup.changeServer() }
                Spacer()
                Button(setup.claimsServer ? "Claim Server" : "Sign In", action: signIn)
                    .buttonStyle(.prominent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!setup.canSignIn)
            }
            .padding(.top, 4)
        }
    }

    private func signIn() {
        Task { await setup.signIn() }
    }
}

struct FolderStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            StepHeader(
                systemImage: "folder",
                title: "Choose a folder to sync",
                help: String(localized: "The folder and the space sync both ways. If the space already has files, Koidrive asks before uploading files that don't match it.")
            )
            VStack(alignment: .leading, spacing: 4) {
                Text("Space")
                    .foregroundStyle(.secondary)
                Picker("Space", selection: $setup.selectedSpaceID) {
                    ForEach(setup.spaces, id: \.spaceId) { space in
                        Text(space.name).tag(Optional(space.spaceId))
                    }
                    if !setup.spaces.isEmpty {
                        Divider()
                    }
                    Text("New Space…").tag(String?.none)
                }
                .labelsHidden()
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            if setup.selectedSpaceID == nil {
                Field(label: "Space name", text: $setup.newSpaceName)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text("Folder")
                    .foregroundStyle(.secondary)
                HStack {
                    Label {
                        Text(verbatim: setup.folderPath.map(abbreviatingHome) ?? String(localized: "None"))
                            .font(.body.monospaced())
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .foregroundStyle(setup.folderPath == nil ? .secondary : .primary)
                    } icon: {
                        Image(systemName: "folder")
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(RoundedRectangle(cornerRadius: 6).fill(.quaternary.opacity(0.6)))
                    Button("Choose…", action: chooseFolder)
                }
            }
            if let error = setup.error {
                ErrorBanner(message: error.message)
            }
            HStack {
                Spacer()
                Button("Link Folder") {
                    Task { await setup.link() }
                }
                .buttonStyle(.prominent)
                .keyboardShortcut(.defaultAction)
                .disabled(!setup.canLink)
            }
            .padding(.top, 4)
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

func abbreviatingHome(_ path: String) -> String {
    (path as NSString).abbreviatingWithTildeInPath
}

private struct DoneStep: View {
    @Environment(AppModel.self) private var app
    let close: () -> Void

    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: "checkmark.circle")
                .font(.system(size: 44, weight: .light))
                .foregroundStyle(Color.statusGreen)
                .padding(.top, 16)
                .padding(.bottom, 8)
            Text("Koidrive is set up")
                .font(.system(size: 17, weight: .semibold))
            Text("Koidrive runs in the menu bar. Add folders in Settings.")
                .foregroundStyle(.secondary)
            ForEach(app.state.folders, id: \.spaceId) { folder in
                HStack(spacing: 8) {
                    Image(systemName: "folder")
                        .foregroundStyle(Color.koi)
                    Text(folder.spaceName)
                        .fontWeight(.semibold)
                    Text(verbatim: abbreviatingHome(folder.path))
                        .font(.callout.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.content))
                .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
                .padding(.top, 8)
            }
            HStack {
                Spacer()
                Button("Done", action: close)
                    .buttonStyle(.prominent)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.top, 8)
        }
        .frame(maxWidth: .infinity)
    }
}

#Preview("Server") {
    SetupView().environment(AppModel.preview(.firstRun))
}

#Preview("Server: usage reports undecided") {
    serverStep(TelemetrySetting(enabled: false, decided: false, locked: false))
}

#Preview("Server: usage reports on") {
    serverStep(TelemetrySetting(enabled: true, decided: true, locked: false))
}

#Preview("Server: usage reports off") {
    serverStep(TelemetrySetting(enabled: false, decided: true, locked: false))
}

#Preview("Server: usage reports locked") {
    serverStep(TelemetrySetting(enabled: false, decided: false, locked: true))
}

@MainActor
private func serverStep(_ telemetry: TelemetrySetting) -> some View {
    let app = AppModel.preview(.firstRun, telemetry: telemetry)
    return SetupSteps(setup: SetupModel(app: app, mode: .firstRun, deviceName: "Mac")).frame(width: 480).environment(app)
}

#Preview("Folder") {
    let app = AppModel.preview(.upToDate)
    let setup = SetupModel(app: app, mode: .addFolder, deviceName: "Mac")
    return FolderStep(setup: setup).padding(24).frame(width: 480).environment(app)
}
