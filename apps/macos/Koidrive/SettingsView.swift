// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import ServiceManagement
import SwiftUI

enum SettingsTab: String {
    case folders, account, storage, problems, general

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
            StorageSettings()
                .tabItem { Label("Storage", systemImage: "internaldrive") }
                .tag(SettingsTab.storage)
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

struct FoldersSettings: View {
    @Environment(AppModel.self) private var model
    @State private var adding: SetupModel?
    @State private var unlinking: LinkedFolder?
    @State private var error: SessionError?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if model.state.folders.isEmpty {
                ContentUnavailableView {
                    Label("No folders", systemImage: "folder")
                } description: {
                    Text(model.state.account == nil ? "Sign in first in the Account tab." : "Add a folder to start syncing.")
                } actions: {
                    addButton
                }
            } else {
                RowGroup {
                    ForEach(Array(model.state.folders.enumerated()), id: \.element.spaceId) { index, folder in
                        if index > 0 { Divider().padding(.leading, 44) }
                        FolderRow(folder: folder) { unlinking = folder }
                    }
                }
                if let error {
                    Label(error.message, systemImage: "exclamationmark.triangle").foregroundStyle(Color.statusRed)
                }
                HStack {
                    Text("Linked folders can't be inside one another.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    addButton
                }
                Spacer()
            }
        }
        .padding(20)
        .sheet(item: $adding) { setup in
            AddFolderSheet(setup: setup) { adding = nil }
        }
        .alert(
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
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Files stay in the folder and on the server.")
        }
    }

    private var addButton: some View {
        Button("Add Folder…") {
            adding = SetupModel(app: model, mode: .addFolder, deviceName: Host.current().localizedName ?? "Mac")
        }
        .disabled(model.state.account == nil)
    }
}

private struct FolderRow: View {
    let folder: LinkedFolder
    let unlink: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "folder")
                .font(.title3)
                .foregroundStyle(Color.koi)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 2) {
                Text(folder.spaceName)
                    .fontWeight(.medium)
                Text(verbatim: abbreviatingHome(folder.path))
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
            HStack(spacing: 5) {
                Circle()
                    .fill(folder.state.color)
                    .frame(width: 7, height: 7)
                Text(folder.state.title)
                    .foregroundStyle(folder.state == .halted ? Color.statusRed : .secondary)
            }
            .font(.callout)
            Button("Show in Finder") {
                NSWorkspace.shared.open(URL(filePath: folder.path, directoryHint: .isDirectory))
            }
            Button("Unlink…", action: unlink)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

extension FolderState {
    var color: Color {
        switch self {
        case .upToDate: .statusGreen
        case .syncing, .recovering: .koi
        case .halted: .statusRed
        case .waiting: .statusYellow
        case .readOnly: .secondary
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

struct AccountSettings: View {
    @Environment(AppModel.self) private var model
    @State private var devices: [Device] = []
    @State private var revoking: Device?
    @State private var pairing: PairingCode?
    @State private var confirmingSignOut = false
    @State private var confirmingLastDevice = false
    @State private var confirmingOffline = false
    @State private var error: SessionError?

    /// `pairing` shows a pairing code from the start, for previews.
    init(pairing: PairingCode? = nil) {
        _pairing = State(initialValue: pairing)
    }

    var body: some View {
        Group {
            if let account = model.state.account {
                ScrollView {
                    VStack(alignment: .leading, spacing: 8) {
                        SectionHeading("Account")
                        RowGroup {
                            SettingsRow {
                                Text("Server").foregroundStyle(.secondary)
                                Spacer()
                                Text(verbatim: account.serverUrl)
                                    .font(.body.monospaced())
                                    .textSelection(.enabled)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                            }
                            if !account.userName.isEmpty {
                                Divider()
                                SettingsRow {
                                    Text("Owner").foregroundStyle(.secondary)
                                    Spacer()
                                    Text(verbatim: account.userName)
                                }
                            }
                        }
                        SectionHeading("Devices")
                            .padding(.top, 8)
                        RowGroup {
                            ForEach(devices, id: \.deviceId) { device in
                                DeviceRow(device: device) { revoking = device }
                                Divider()
                            }
                            SettingsRow {
                                if let pairing {
                                    PairingCodeLabel(code: pairing.code)
                                    Text("Enter it on the new device. Expires \(Date(milliseconds: pairing.expiresAtMs), format: .dateTime.hour().minute()).")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                        .fixedSize(horizontal: false, vertical: true)
                                }
                                Spacer()
                                Button("Add a Device…") {
                                    perform { pairing = try await model.startPairing() }
                                }
                            }
                        }
                        if let error {
                            Label(error.message, systemImage: "exclamationmark.triangle").foregroundStyle(Color.statusRed)
                        }
                        HStack {
                            Spacer()
                            Button("Sign Out…") { confirmingSignOut = true }
                                .foregroundStyle(Color.statusRed)
                        }
                        .padding(.top, 8)
                    }
                    .padding(20)
                }
                .task(id: account.deviceId) {
                    perform { devices = try await model.devices() }
                }
            } else {
                ContentUnavailableView("Not signed in", systemImage: "person.crop.circle.badge.questionmark")
            }
        }
        .alert(
            "Remove \(revoking?.name ?? "")?",
            isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }),
            presenting: revoking
        ) { device in
            Button("Remove", role: .destructive) {
                perform {
                    try await model.revokeDevice(device)
                    devices.removeAll { $0.deviceId == device.deviceId }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("It stops syncing. Files already on it stay there.")
        }
        .alert("Sign out of Koidrive?", isPresented: $confirmingSignOut) {
            Button("Sign Out", role: .destructive) {
                perform {
                    do {
                        try await model.signOut()
                    } catch SessionError.LastDevice {
                        confirmingLastDevice = true
                    } catch SessionError.NotRevoked(_) {
                        confirmingOffline = true
                    }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This Mac stops syncing and unlinks every folder. Files stay in the folders and on the server.")
        }
        .alert("Sign out of your last device?", isPresented: $confirmingLastDevice) {
            Button("Sign Out", role: .destructive) {
                perform { try await model.signOut(confirmLastDevice: true) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("No other device is signed in. To get back in, run koi server add-device on any computer, as the guide's Devices page describes.")
        }
        .alert("Sign out without the server?", isPresented: $confirmingOffline) {
            Button("Sign Out", role: .destructive) {
                perform { try await model.signOut(confirmLastDevice: true) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Koidrive can't reach the server to check for other devices, and this Mac stays listed there until another device removes it. If it's the last device, run koi server add-device to get back in.")
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

private struct SectionHeading: View {
    let title: LocalizedStringKey

    init(_ title: LocalizedStringKey) {
        self.title = title
    }

    var body: some View {
        Text(title)
            .fontWeight(.semibold)
            .foregroundStyle(.secondary)
    }
}

/// One row of a settings group.
private struct SettingsRow<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: 10) {
            content
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

private struct DeviceRow: View {
    let device: Device
    let remove: () -> Void

    var body: some View {
        SettingsRow {
            Image(systemName: "laptopcomputer")
                .foregroundStyle(.secondary)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 4) {
                    Text(verbatim: device.name)
                    if device.thisDevice {
                        Text("(this Mac)").foregroundStyle(.secondary)
                    }
                }
                Text("\(device.platformName) · Added \(Date(milliseconds: device.addedAtMs), format: .dateTime.day().month(.abbreviated).year())")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if !device.thisDevice {
                Button("Remove…", action: remove)
                    .foregroundStyle(Color.statusRed)
            }
        }
    }
}

extension Device {
    var platformName: String {
        switch platform {
        case "macos": "macOS"
        case "linux": "Linux"
        case "windows": "Windows"
        default: platform
        }
    }
}

private struct PairingCodeLabel: View {
    let code: String
    @State private var copied = false

    var body: some View {
        HStack(spacing: 8) {
            Text("Pairing code")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(verbatim: code)
                .font(.body.monospaced().weight(.semibold))
                .textSelection(.enabled)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(code, forType: .string)
                copied = true
            } label: {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .help("Copy")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(RoundedRectangle(cornerRadius: 6).fill(.quaternary.opacity(0.6)))
        .fixedSize()
        .onChange(of: code) { copied = false }
    }
}

// MARK: Storage

struct StorageSettings: View {
    @Environment(AppModel.self) private var model
    @State private var storage = StorageModel()

    var body: some View {
        Group {
            if let account = model.state.account {
                Form {
                    let spaces = storage.spaces ?? []
                    if spaces.count == 1 {
                        Section { usage(of: spaces[0]) }
                    } else {
                        ForEach(spaces) { entry in
                            Section(entry.space.name) { usage(of: entry) }
                        }
                    }
                    Section {
                        if storage.spaces?.isEmpty == true {
                            Text("This account has no spaces yet.").foregroundStyle(.secondary)
                        }
                        if let error = storage.error {
                            Label(error.message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                        }
                        HStack {
                            Text("Each stored chunk counts once, under the first of live files, trash and old versions that holds it. Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Spacer()
                            if storage.isLoading {
                                ProgressView().controlSize(.small)
                            } else {
                                Button("Refresh") { Task { await storage.load(model.storage) } }
                            }
                        }
                    }
                }
                .formStyle(.grouped)
                .task(id: account.deviceId) { await storage.load(model.storage) }
            } else {
                ContentUnavailableView("Not signed in", systemImage: "person.crop.circle.badge.questionmark")
            }
        }
    }

    /// A space's usage rows, or why the server refused them. With a single space its section has
    /// no header.
    @ViewBuilder
    private func usage(of entry: SpaceStorage) -> some View {
        switch entry.usage {
        case .success(let usage):
            StorageRow(title: "Live files", count: "^[\(usage.liveFiles) file](inflect: true)", bytes: usage.liveBytes)
            StorageRow(title: "Trash", count: "^[\(usage.trashedFiles) file](inflect: true)", bytes: usage.trashBytes)
            StorageRow(title: "Old versions", count: "^[\(usage.oldVersions) version](inflect: true)", bytes: usage.oldVersionBytes)
            StorageRow(title: "Unreferenced", count: nil, bytes: usage.unreferencedBytes)
            StorageRow(title: "Total", count: nil, bytes: usage.totalBytes).bold()
        case .failure(let refusal):
            Label(refusal.message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
        }
    }
}

private struct StorageRow: View {
    let title: LocalizedStringKey
    let count: LocalizedStringKey?
    let bytes: UInt64

    var body: some View {
        LabeledContent(title) {
            HStack(spacing: 12) {
                if let count {
                    Text(count).foregroundStyle(.secondary)
                }
                Text(Int64(clamping: bytes), format: .byteCount(style: .file))
                    .monospacedDigit()
                    .frame(minWidth: 70, alignment: .trailing)
            }
        }
    }
}

// MARK: Problems

struct ProblemsSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if model.state.problems.isEmpty {
            ContentUnavailableView("No problems", systemImage: "checkmark.circle")
        } else {
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(model.problemSections) { section in
                        SpaceHeading(spaceID: section.spaceID, folder: section.folder)
                            .fontWeight(.semibold)
                            .foregroundStyle(.secondary)
                            .padding(.top, section.id == model.problemSections.first?.id ? 0 : 8)
                        RowGroup {
                            ForEach(Array(section.problems.enumerated()), id: \.element.id) { index, problem in
                                if index > 0 { Divider() }
                                ProblemRow(problem: problem, folder: section.folder)
                            }
                        }
                    }
                }
                .padding(20)
            }
        }
    }
}

private struct ProblemRow: View {
    let problem: Problem
    let folder: LinkedFolder?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .foregroundStyle(Color.statusRed)
            VStack(alignment: .leading, spacing: 4) {
                Text(problem.kind.title)
                    .fontWeight(.semibold)
                if let path = problem.path {
                    Text(verbatim: path)
                        .font(.callout.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                } else if let folder, problem.kind.concernsTheFolder {
                    Text(verbatim: abbreviatingHome(folder.path))
                        .font(.callout.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
                Text(problem.kind.advice)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text("Since \(Date(milliseconds: problem.sinceMs), format: .dateTime.day().month(.abbreviated).hour().minute())")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if let folder {
                Button("Show in Finder") {
                    let url = URL(filePath: folder.path)
                    if let path = problem.path {
                        NSWorkspace.shared.activateFileViewerSelecting([url.appending(path: path)])
                    } else if FileManager.default.fileExists(atPath: folder.path) {
                        NSWorkspace.shared.open(url)
                    } else {
                        var parent = url.deletingLastPathComponent()
                        while !FileManager.default.fileExists(atPath: parent.path(percentEncoded: false)) {
                            parent.deleteLastPathComponent()
                        }
                        NSWorkspace.shared.open(parent)
                    }
                }
            }
        }
        .padding(12)
    }
}

extension ProblemKind {
    /// Whether the problem is about the linked folder itself, so its path says where.
    var concernsTheFolder: Bool {
        switch self {
        case .folderMissing, .folderNotAFolder, .folderUnreadable, .folderOnOtherVolume, .markerMissing,
             .markerUnreadable, .markerOfOtherSpace, .diskOlderThanSyncState, .unsupportedFileSystem:
            true
        default:
            false
        }
    }
}

// MARK: General

struct GeneralSettings: View {
    @State private var loginItemStatus = SMAppService.mainApp.status
    @State private var openAtLogin = false
    @State private var loginItemError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            RowGroup {
                SettingsRow {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Open Koidrive at login")
                        if loginItemStatus == .requiresApproval {
                            Text("Allow Koidrive in Login Items.")
                                .font(.caption)
                                .foregroundStyle(Color.koiText)
                        }
                        if let loginItemError {
                            Text(loginItemError)
                                .font(.caption)
                                .foregroundStyle(Color.statusRed)
                        }
                    }
                    Spacer()
                    if loginItemStatus == .requiresApproval {
                        Button("Open Login Items") { SMAppService.openSystemSettingsLoginItems() }
                    }
                    Toggle("Open Koidrive at login", isOn: $openAtLogin)
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .tint(.koi)
                        .onChange(of: openAtLogin) { _, on in
                            if on != isRegistered {
                                setOpenAtLogin(on)
                            }
                        }
                }
            }
            SectionHeading("About")
                .padding(.top, 8)
            RowGroup {
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: "Koidrive " + (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? ""))
                    Text("Command-line tool: koi")
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .help(Text(verbatim: "Core \(KoidriveFFI.version())"))
                Divider()
                Text(independenceNotice)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
            }
            Spacer()
        }
        .padding(20)
        .onAppear(perform: readStatus)
    }

    private var isRegistered: Bool {
        loginItemStatus == .enabled || loginItemStatus == .requiresApproval
    }

    private func readStatus() {
        loginItemStatus = SMAppService.mainApp.status
        openAtLogin = isRegistered
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
        readStatus()
    }
}

#Preview("Folders") {
    SettingsView().environment(AppModel.preview(.upToDate))
}

#Preview("Problems") {
    ProblemsSettings().environment(AppModel.preview(.problems)).frame(width: 600, height: 440)
}

#Preview("Storage: one space") {
    StorageSettings().environment(AppModel.previewWithOneSpace()).frame(width: 600, height: 440)
}

#Preview("Storage: two spaces") {
    StorageSettings().environment(AppModel.preview(.upToDate)).frame(width: 600, height: 440)
}

#Preview("Account") {
    AccountSettings().environment(AppModel.preview(.upToDate)).frame(width: 600, height: 440)
}
