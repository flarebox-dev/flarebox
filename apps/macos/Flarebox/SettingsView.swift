// SPDX-License-Identifier: Apache-2.0

import AppKit
import FlareboxFFI
import FlareboxModel
import ServiceManagement
import SwiftUI

enum SettingsTab: String {
    case folders, account, problems, general

    static let storageKey = "settingsTab"
}

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @AppStorage(SettingsTab.storageKey) private var tab = SettingsTab.folders

    var body: some View {
        TabView(selection: $tab) {
            FoldersSettings()
                .tabItem { Label("Folders", systemImage: "folder") }
                .tag(SettingsTab.folders)
            AccountSettings()
                .tabItem { Label("Account", systemImage: "person.crop.circle") }
                .tag(SettingsTab.account)
            ProblemsSettings()
                .tabItem { Label("Problems", systemImage: "exclamationmark.triangle") }
                .badge(model.state.problems.count)
                .tag(SettingsTab.problems)
            GeneralSettings()
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(SettingsTab.general)
        }
        .frame(width: 600, height: 440)
    }
}

// MARK: Folders

private struct FoldersSettings: View {
    @Environment(AppModel.self) private var model
    @State private var adding: SetupModel?
    @State private var unlinking: LinkedFolder?
    @State private var error: SessionError?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if model.state.folders.isEmpty {
                ContentUnavailableView("No Linked Folders", systemImage: "folder.badge.questionmark")
            } else {
                List(model.state.folders, id: \.spaceId) { folder in
                    HStack {
                        Image(systemName: "folder.fill").foregroundStyle(.tint)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(folder.spaceName).font(.headline)
                            Text(folder.path)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                        Spacer()
                        Text(folder.state.title)
                            .font(.caption)
                            .foregroundStyle(folder.state == .halted ? .red : .secondary)
                        Button("Show in Finder") {
                            NSWorkspace.shared.open(URL(filePath: folder.path, directoryHint: .isDirectory))
                        }
                        Button("Unlink…") { unlinking = folder }
                    }
                    .padding(.vertical, 4)
                }
            }
            if let error {
                Label(error.message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Add Folder…") {
                    adding = SetupModel(app: model, deviceName: Host.current().localizedName ?? "Mac")
                }
                .disabled(model.state.account == nil)
            }
        }
        .padding(20)
        .sheet(item: $adding) { setup in
            AddFolderSheet(setup: setup) { adding = nil }
        }
        .confirmationDialog(
            "Unlink \(unlinking?.spaceName ?? "")?",
            isPresented: Binding(get: { unlinking != nil }, set: { if !$0 { unlinking = nil } }),
            presenting: unlinking
        ) { folder in
            Button("Unlink", role: .destructive) {
                Task {
                    do {
                        try await model.unlinkFolder(spaceID: folder.spaceId)
                        error = nil
                    } catch let failure as SessionError {
                        error = failure
                    } catch {}
                }
            }
        } message: { _ in
            Text("Flarebox stops syncing this folder on this Mac. The files stay in the folder and on the server.")
        }
    }
}

extension SetupModel: @retroactive Identifiable {}

private struct AddFolderSheet: View {
    @Bindable var setup: SetupModel
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            FolderStep(setup: setup)
            if let error = setup.error {
                Label(error.message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
            }
            HStack {
                Button("Cancel", action: close).keyboardShortcut(.cancelAction)
                Spacer()
            }
        }
        .padding(24)
        .frame(width: 480)
        .disabled(setup.isBusy)
        .onChange(of: setup.step) { _, step in
            if step == .done { close() }
        }
    }
}

// MARK: Account

private struct AccountSettings: View {
    @Environment(AppModel.self) private var model
    @State private var devices: DeviceList?
    @State private var pairing: PairingCode?
    @State private var confirmingSignOut = false
    @State private var error: SessionError?

    var body: some View {
        Group {
            if let account = model.state.account {
                Form {
                    Section("Account") {
                        LabeledContent("Server", value: account.serverUrl)
                        if !account.userName.isEmpty {
                            LabeledContent("Owner", value: account.userName)
                        }
                        LabeledContent("This Mac", value: account.deviceName)
                    }
                    Section("Devices") {
                        if devices == .unavailable {
                            Text("This server can't list the account's devices yet.")
                                .foregroundStyle(.secondary)
                        }
                        ForEach(devices?.available ?? [], id: \.deviceId) { device in
                            LabeledContent {
                                Text("Added \(Date(milliseconds: device.addedAtMs), format: .dateTime.year().month().day())")
                            } label: {
                                Label {
                                    Text(device.thisDevice ? String(localized: "\(device.name) (this Mac)") : device.name)
                                } icon: {
                                    Image(systemName: device.platform == "macos" ? "desktopcomputer" : "server.rack")
                                }
                            }
                        }
                        if let pairing {
                            LabeledContent("Pairing code") {
                                VStack(alignment: .trailing) {
                                    Text(pairing.code)
                                        .font(.title3.monospaced())
                                        .textSelection(.enabled)
                                    Text("Expires \(Date(milliseconds: pairing.expiresAtMs), format: .dateTime.hour().minute())")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                        }
                        HStack {
                            Text("Enter the pairing code on the new device while setting it up.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Spacer()
                            Button("Add a Device…") {
                                perform { pairing = try await model.startPairing() }
                            }
                        }
                    }
                    Section {
                        if let error {
                            Label(error.message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                        }
                        Button("Sign Out…", role: .destructive) { confirmingSignOut = true }
                    }
                }
                .formStyle(.grouped)
                .task(id: account.deviceId) {
                    perform { devices = try await model.devices() }
                }
            } else {
                ContentUnavailableView("Not Signed In", systemImage: "person.crop.circle.badge.questionmark")
            }
        }
        .confirmationDialog("Sign out of Flarebox?", isPresented: $confirmingSignOut) {
            Button("Sign Out", role: .destructive) {
                perform { try await model.signOut() }
            }
        } message: {
            Text("This Mac stops syncing and unlinks every folder. The files stay in the folders and on the server.")
        }
    }

    private func perform(_ body: @escaping @MainActor () async throws -> Void) {
        Task {
            do {
                try await body()
                error = nil
            } catch let failure as SessionError {
                error = failure
            } catch {}
        }
    }
}

extension DeviceList {
    fileprivate var available: [Device]? {
        if case .available(let devices) = self { devices } else { nil }
    }
}

// MARK: Problems

private struct ProblemsSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if model.state.problems.isEmpty {
            ContentUnavailableView("No Problems", systemImage: "checkmark.circle")
        } else {
            List {
                ForEach(model.problemSections) { section in
                    Section(section.folder?.spaceName ?? section.spaceID ?? String(localized: "Flarebox")) {
                        ForEach(section.problems) { problem in
                            ProblemRow(problem: problem, folder: section.folder)
                        }
                    }
                }
            }
        }
    }
}

private struct ProblemRow: View {
    let problem: Problem
    let folder: LinkedFolder?

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Label(problem.kind.title, systemImage: "exclamationmark.triangle.fill")
                .font(.headline)
                .foregroundStyle(.red)
            if let path = problem.path {
                Text(path)
                    .font(.callout.monospaced())
                    .textSelection(.enabled)
            }
            Text(problem.kind.advice)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Text("Since \(Date(milliseconds: problem.sinceMs), format: .dateTime.month().day().hour().minute())")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                if let folder, let path = problem.path {
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(filePath: folder.path).appending(path: path)])
                    }
                    .controlSize(.small)
                }
            }
        }
        .padding(.vertical, 4)
    }
}

// MARK: General

private struct GeneralSettings: View {
    @State private var loginItemStatus = SMAppService.mainApp.status
    @State private var loginItemError: String?

    var body: some View {
        Form {
            Section {
                Toggle("Open Flarebox at login", isOn: Binding(
                    get: { loginItemStatus == .enabled || loginItemStatus == .requiresApproval },
                    set: setOpenAtLogin
                ))
                if loginItemStatus == .requiresApproval {
                    HStack {
                        Text("Allow Flarebox in Login Items to finish.")
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button("Open Login Items") { SMAppService.openSystemSettingsLoginItems() }
                    }
                }
                if let loginItemError {
                    Label(loginItemError, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                }
            }
            Section("About") {
                LabeledContent("Version", value: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
                LabeledContent("Core", value: FlareboxFFI.version())
                Text(independenceNotice)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear { loginItemStatus = SMAppService.mainApp.status }
    }

    private func setOpenAtLogin(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            loginItemError = nil
        } catch {
            loginItemError = error.localizedDescription
        }
        loginItemStatus = SMAppService.mainApp.status
    }
}

#Preview("Folders") {
    SettingsView().environment(AppModel.preview(.upToDate))
}

#Preview("Problems") {
    ProblemsSettings().environment(AppModel.preview(.problems)).frame(width: 600, height: 440)
}

#Preview("Account") {
    AccountSettings().environment(AppModel.preview(.upToDate)).frame(width: 600, height: 440)
}
