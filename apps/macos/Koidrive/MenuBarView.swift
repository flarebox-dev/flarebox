// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

/// The menu bar icon: the koi mark as a template image, with a badge for the state. It stays
/// alive while the app runs, so it also opens the setup window on a first run and the questions
/// window whenever there are questions the person has not seen, including the unanswered ones
/// after a relaunch.
struct MenuBarLabel: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Image(model.iconBadge.imageName)
            .accessibilityLabel(Text("Koidrive: \(model.headline == .questions ? String(localized: "Needs you") : model.headlineTitle)"))
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

extension IconBadge {
    /// The template image in the asset catalog, from the GUN-420 handoff.
    var imageName: String {
        switch self {
        case .none: "MenuBar/up-to-date"
        case .syncing: "MenuBar/syncing"
        case .paused: "MenuBar/paused"
        case .offline: "MenuBar/offline"
        case .question: "MenuBar/question"
        case .problem: "MenuBar/problem"
        }
    }
}

extension AppModel {
    /// The popover title, which the menu bar icon also reads out.
    var headlineTitle: String {
        switch headline {
        case .setup: SyncStatus.needsSetup.title
        case .halted: SyncStatus.halted.title
        case .questions: String(localized: "Needs your answer")
        case .status(let status): status.title
        }
    }

    /// The line under the popover title, if any.
    var headlineSubtitle: String? {
        switch headline {
        case .setup:
            return String(localized: "Connect a server and choose a folder.")
        case .halted:
            let stopped = state.folders.filter { $0.state == .halted }.map { folder in
                let missing = state.problems.contains { $0.spaceId == folder.spaceId && $0.kind == .folderMissing }
                return missing
                    ? String(localized: "\(folder.spaceName) is missing.")
                    : String(localized: "\(folder.spaceName) stopped.")
            }
            let others = state.folders.filter { $0.state != .halted }
            let rest: String? = switch others.count {
            case 0: nil
            case 1: String(localized: "\(others[0].spaceName) still syncs.")
            default: String(localized: "The other folders still sync.")
            }
            return (stopped + [rest].compactMap { $0 }).joined(separator: " ")
        case .questions:
            return state.status == .idle ? String(localized: "Everything else is up to date.") : state.status.title
        case .status(.paused):
            return String(localized: "Changes sync when you resume.")
        case .status(.offline):
            guard let server = state.account?.serverUrl else { return nil }
            let host = URL(string: server)?.host() ?? server
            return String(localized: "Retrying \(host). Changes sync when it's back.")
        case .status:
            return nil
        }
    }

    var headlineSymbol: (name: String, color: Color) {
        switch headline {
        case .setup: ("arrow.triangle.2.circlepath", .secondary)
        case .halted: ("exclamationmark.triangle", .statusRed)
        case .questions: ("questionmark.circle", .koiText)
        case .status(.idle): ("checkmark.circle", .statusGreen)
        case .status(.syncing), .status(.connecting): ("arrow.triangle.2.circlepath", .koi)
        case .status(.paused), .status(.stopped): ("pause", .secondary)
        case .status(.offline): ("bolt.circle", .statusYellow)
        case .status(.needsSetup): ("arrow.triangle.2.circlepath", .secondary)
        case .status(.halted): ("exclamationmark.triangle", .statusRed)
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
        .tint(.koi)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label {
                Text(model.headlineTitle)
            } icon: {
                Image(systemName: model.headlineSymbol.name)
                    .foregroundStyle(model.headlineSymbol.color)
            }
            .font(.system(size: 15, weight: .semibold))
            if let subtitle = model.headlineSubtitle {
                Text(subtitle)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let progress = model.state.progress, model.headline == .status(.syncing) {
                if let fraction = model.progressFraction {
                    ProgressBar(fraction: fraction)
                }
                Text("\(progress.filesToUpload.formatted()) to upload, \(progress.filesToDownload.formatted()) to download · \(byteProgress(done: progress.bytesDone, total: progress.bytesTotal))")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                if let current = progress.current {
                    Text(verbatim: model.displayPath(current.path, spaceID: current.spaceId))
                        .font(.caption.monospaced())
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
                text: "\(model.state.questions.count) need you",
                systemImage: "questionmark.circle",
                count: nil,
                tint: .koiText,
                wash: .koiTint,
                badge: .koi,
                badgeText: .onKoi
            ) {
                openWindow.show(WindowID.questions)
            }
        }
        if !model.state.problems.isEmpty {
            AttentionRow(
                text: "Review problems",
                systemImage: "exclamationmark.triangle",
                count: model.state.problems.count,
                tint: .statusRed,
                wash: .redTint,
                badge: .redFill,
                badgeText: .white
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
            Label(error.message, systemImage: "exclamationmark.triangle")
                .font(.callout)
                .foregroundStyle(Color.statusRed)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder
    private var recentChanges: some View {
        if !model.state.recentChanges.isEmpty {
            let busy = model.headline != .status(.idle) || !model.state.notices.isEmpty || !model.state.problems.isEmpty
            VStack(alignment: .leading, spacing: 6) {
                Text("Recent changes")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
                ForEach(Array(model.state.recentChanges.prefix(busy ? 3 : 5).enumerated()), id: \.offset) { _, change in
                    RecentChangeRow(change: change, folder: model.folder(spaceID: change.spaceId))
                }
            }
        }
    }

    private var actions: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(model.state.folders, id: \.spaceId) { folder in
                MenuButton(title: "Open \(folder.spaceName)", systemImage: "folder") {
                    NSWorkspace.shared.open(URL(filePath: folder.path, directoryHint: .isDirectory))
                }
            }
            if model.needsSetup {
                MenuButton(title: "Set Up Koidrive…", systemImage: "wand.and.stars") {
                    openWindow.show(WindowID.setup)
                }
            } else {
                MenuButton(
                    title: model.isPaused ? "Resume Sync" : "Pause Sync",
                    systemImage: model.isPaused ? "play" : "pause"
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
            MenuButton(title: "Needs You…", systemImage: "questionmark.circle") {
                openWindow.show(WindowID.questions)
            }
            MenuButton(title: "Settings…", systemImage: "gearshape", shortcut: "⌘,") {
                openSettings()
                NSApp.activate()
            }
            Divider()
                .padding(.vertical, 6)
            MenuButton(title: "Quit Koidrive", systemImage: "power", shortcut: "⌘Q") {
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
    }

    /// "148 of 240 MB": the done bytes in the total's unit.
    private func byteProgress(done: UInt64, total: UInt64) -> String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        let units: [ByteCountFormatter.Units] = [.useBytes, .useKB, .useMB, .useGB, .useTB]
        let power = min(units.count - 1, total < 1000 ? 0 : Int(log10(Double(total)) / 3))
        formatter.allowedUnits = units[power]
        let totalText = formatter.string(fromByteCount: Int64(clamping: total))
        formatter.includesUnit = false
        let doneText = formatter.string(fromByteCount: Int64(clamping: done))
        return String(localized: "\(doneText) of \(totalText)")
    }
}

extension AppModel {
    /// A path inside a space as the person knows it, starting with the folder's name.
    func displayPath(_ path: String, spaceID: String) -> String {
        guard let folder = folder(spaceID: spaceID) else { return path }
        return folder.spaceName + "/" + path
    }
}

/// A thin koi-orange bar on a grey track. The system's linear progress view takes the
/// system accent colour on macOS, not the tint.
private struct ProgressBar: View {
    let fraction: Double

    var body: some View {
        Capsule()
            .fill(Color.primary.opacity(0.1))
            .frame(height: 6)
            .overlay(alignment: .leading) {
                GeometryReader { geometry in
                    Capsule()
                        .fill(Color.koi)
                        .frame(width: geometry.size.width * fraction)
                }
            }
            .accessibilityElement()
            .accessibilityValue(Text(fraction, format: .percent.precision(.fractionLength(0))))
            .padding(.vertical, 2)
    }
}

private struct AttentionRow: View {
    let text: LocalizedStringKey
    let systemImage: String
    let count: Int?
    let tint: Color
    let wash: Color
    let badge: Color
    let badgeText: Color
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack {
                Label(text, systemImage: systemImage)
                    .foregroundStyle(tint)
                    .font(.body.weight(.medium))
                Spacer()
                if let count {
                    CountBadge(count: count, fill: badge, text: badgeText)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(RoundedRectangle(cornerRadius: 8).fill(wash))
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
            Image(systemName: notice.symbolName)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: [notice.title, folder?.spaceName].compactMap { $0 }.joined(separator: " · "))
                    .font(.callout.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                Text(notice.text)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button(action: dismiss) {
                Image(systemName: "xmark")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 8).fill(.quaternary.opacity(0.6)))
    }
}

extension Notice {
    var symbolName: String {
        switch kind {
        case .recovery: "clock.arrow.circlepath"
        case .remoteMassDelete: "trash"
        }
    }
}

private struct RecentChangeRow: View {
    let change: RecentChange
    let folder: LinkedFolder?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: change.direction == .uploaded ? "arrow.up" : "arrow.down")
                .font(.caption)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(verbatim: folder.map { $0.spaceName + "/" + change.path } ?? change.path)
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
    var shortcut: String?
    let action: () -> Void
    @State private var hovering = false
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            HStack {
                Label {
                    Text(title)
                } icon: {
                    Image(systemName: systemImage)
                        .foregroundStyle(.secondary)
                        .frame(width: 16)
                }
                Spacer()
                if let shortcut {
                    Text(verbatim: shortcut)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(RoundedRectangle(cornerRadius: 6).fill(hovering && isEnabled ? Color.primary.opacity(0.1) : .clear))
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
