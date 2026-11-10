// SPDX-License-Identifier: Apache-2.0

import AppKit
import FlareboxFFI
import FlareboxModel
import SwiftUI

/// The menu bar icon. It stays alive while the app runs, so it also opens the setup window on a
/// first run and the questions window whenever there are questions the person has not seen,
/// including the unanswered ones after a relaunch.
struct MenuBarLabel: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Image(systemName: model.attentionCount > 0 ? "exclamationmark.arrow.triangle.2.circlepath" : model.state.status.symbolName)
            .accessibilityLabel(Text("Flarebox: \(model.state.status.title)"))
            .task {
                if model.needsSetup {
                    openWindow.show(WindowID.setup)
                }
            }
            .onChange(of: model.hasUnseenQuestions, initial: true) { _, unseen in
                if unseen {
                    openWindow.show(WindowID.questions)
                }
            }
    }
}

struct MenuContent: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings
    @AppStorage(SettingsTab.storageKey) private var settingsTab = SettingsTab.folders
    @State private var error: SessionError?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            attention
            notices
            recentChanges
            Divider()
            actions
        }
        .padding(14)
        .frame(width: 330)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(model.state.status.title, systemImage: model.state.status.symbolName)
                .font(.headline)
            if let progress = model.state.progress {
                if let fraction = model.progressFraction {
                    ProgressView(value: fraction)
                }
                Text("\(progress.filesToUpload.formatted()) to upload, \(progress.filesToDownload.formatted()) to download · \(Int64(progress.bytesDone).formatted(.byteCount(style: .file))) of \(Int64(progress.bytesTotal).formatted(.byteCount(style: .file)))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if let current = progress.current {
                    Text(current.path)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
        }
    }

    @ViewBuilder
    private var attention: some View {
        if !model.state.questions.isEmpty {
            AttentionRow(
                text: "\(model.state.questions.count) questions need an answer",
                systemImage: "questionmark.circle.fill",
                count: model.state.questions.count,
                tint: .orange
            ) {
                openWindow.show(WindowID.questions)
            }
        }
        if !model.state.problems.isEmpty {
            AttentionRow(
                text: "\(model.state.problems.count) problems",
                systemImage: "exclamationmark.triangle.fill",
                count: model.state.problems.count,
                tint: .red
            ) {
                settingsTab = .problems
                openSettings()
                NSApp.activate()
            }
        }
    }

    @ViewBuilder
    private var notices: some View {
        ForEach(model.state.notices) { notice in
            NoticeRow(notice: notice, folder: model.folder(spaceID: notice.spaceId)) {
                Task {
                    do {
                        try await model.dismiss(notice)
                    } catch let failure as SessionError {
                        error = failure
                    } catch {}
                }
            }
        }
        if let error {
            Label(error.message, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder
    private var recentChanges: some View {
        if !model.state.recentChanges.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("Recent changes")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                ForEach(Array(model.state.recentChanges.prefix(5).enumerated()), id: \.offset) { _, change in
                    RecentChangeRow(change: change, folder: model.folder(spaceID: change.spaceId))
                }
            }
        }
    }

    private var actions: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(model.state.folders, id: \.spaceId) { folder in
                MenuButton(title: "Open \(folder.spaceName)", systemImage: "folder") {
                    NSWorkspace.shared.open(URL(filePath: folder.path, directoryHint: .isDirectory))
                }
            }
            if model.needsSetup {
                MenuButton(title: "Set Up Flarebox…", systemImage: "wand.and.stars") {
                    openWindow.show(WindowID.setup)
                }
            } else {
                MenuButton(
                    title: model.isPaused ? "Resume Sync" : "Pause Sync",
                    systemImage: model.isPaused ? "play.fill" : "pause.fill"
                ) {
                    do {
                        try model.togglePause()
                        error = nil
                    } catch let failure as SessionError {
                        error = failure
                    } catch {}
                }
                .disabled(model.state.status == .stopped)
            }
            MenuButton(title: "Questions…", systemImage: "questionmark.circle") {
                openWindow.show(WindowID.questions)
            }
            MenuButton(title: "Settings…", systemImage: "gearshape") {
                openSettings()
                NSApp.activate()
            }
            Divider()
            MenuButton(title: "Quit Flarebox", systemImage: "power") {
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
    }
}

private struct AttentionRow: View {
    let text: LocalizedStringKey
    let systemImage: String
    let count: Int
    let tint: Color
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack {
                Label(text, systemImage: systemImage)
                    .foregroundStyle(tint)
                Spacer()
                Text(count.formatted())
                    .font(.caption.weight(.bold))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(Capsule().fill(tint))
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

private struct NoticeRow: View {
    let notice: Notice
    let folder: LinkedFolder?
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "info.circle.fill")
                .foregroundStyle(.blue)
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: [notice.title, folder?.spaceName].compactMap { $0 }.joined(separator: " · "))
                    .font(.callout.weight(.semibold))
                Text(notice.text)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button(action: dismiss) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 8).fill(.quaternary))
    }
}

private struct RecentChangeRow: View {
    let change: RecentChange
    let folder: LinkedFolder?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: change.direction == .uploaded ? "arrow.up.circle" : "arrow.down.circle")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(change.path)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("\(change.summary) · \(Date(milliseconds: change.atMs), format: .relative(presentation: .named))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .onTapGesture(count: 2) {
            guard let folder, change.change != .deleted else { return }
            NSWorkspace.shared.activateFileViewerSelecting([URL(filePath: folder.path).appending(path: change.path)])
        }
    }
}

private struct MenuButton: View {
    let title: LocalizedStringKey
    let systemImage: String
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: systemImage)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 6)
                .padding(.vertical, 3)
                .background(RoundedRectangle(cornerRadius: 5).fill(hovering ? Color.accentColor.opacity(0.2) : .clear))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

#Preview("Syncing") {
    MenuContent().environment(AppModel.preview(.syncing))
}

#Preview("Questions and problems") {
    let model = AppModel.preview(.massDelete)
    _ = try? model.session.fakeCore()?.reportProblem(spaceId: nil, path: nil, kind: .signInRequired)
    return MenuContent().environment(model)
}

#Preview("Remote mass delete") {
    MenuContent().environment(AppModel.preview(.remoteMassDelete))
}

#Preview("First run") {
    MenuContent().environment(AppModel.preview(.firstRun))
}
