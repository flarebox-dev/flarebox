// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

struct QuestionsView: View {
    @Environment(AppModel.self) private var model
    @State private var selection: String?

    var body: some View {
        NavigationSplitView {
            List(selection: $selection) {
                if model.questionSectionsShowSpace {
                    ForEach(model.questionSections) { section in
                        Section {
                            rows(of: section)
                        } header: {
                            SpaceHeading(spaceID: section.spaceID, folder: section.folder)
                        }
                    }
                } else {
                    ForEach(model.questionSections) { section in
                        rows(of: section)
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 240, ideal: 260)
        } detail: {
            if let question = model.state.questions.first(where: { $0.id == selection }) {
                QuestionDetail(question: question, folder: model.folder(spaceID: question.spaceId))
                    .id(question.id)
                    .frame(minWidth: 480, minHeight: 360)
            } else {
                ContentUnavailableView(
                    "Nothing needs you",
                    systemImage: "checkmark.circle",
                    description: Text("Decisions Koidrive needs from you appear here.")
                )
            }
        }
        .navigationTitle("Needs You")
        .onAppear(perform: seen)
        .onChange(of: model.unseenQuestionIDs) { seen() }
        .onChange(of: model.state.questions.map(\.id), initial: true) { _, ids in
            if selection == nil || !ids.contains(selection!) {
                selection = model.questionSections.first?.questions.first?.id
            }
        }
    }

    @ViewBuilder
    private func rows(of section: QuestionSection) -> some View {
        if let cause = section.recoveryCause {
            Label(cause.shortText, systemImage: "clock")
                .font(.caption)
                .foregroundStyle(.secondary)
                .selectionDisabled()
        }
        ForEach(section.questions) { question in
            QuestionRow(question: question, selected: selection == question.id)
                .tag(question.id)
        }
    }

    private func seen() {
        model.markQuestionsSeen()
    }
}

struct SpaceHeading: View {
    let spaceID: String?
    let folder: LinkedFolder?

    var body: some View {
        if let folder {
            Text(folder.spaceName)
        } else if let spaceID {
            HStack(spacing: 6) {
                Text("Unlinked space")
                Text(verbatim: spaceID)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        } else {
            Text(verbatim: "Koidrive")
        }
    }
}

private struct QuestionRow: View {
    let question: Question
    let selected: Bool

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(question.title)
                    .fontWeight(selected ? .semibold : .regular)
                Text(verbatim: "\(itemCount(question.items.count)) · \(fileCount(question.affectedFiles))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } icon: {
            Image(systemName: question.symbolName)
                .foregroundStyle(selected ? Color.koiText : .secondary)
        }
        .padding(.vertical, 2)
    }
}

private struct QuestionDetail: View {
    @Environment(AppModel.self) private var model
    let question: Question
    let folder: LinkedFolder?
    @State private var confirming: Answer?
    @State private var error: SessionError?
    @State private var answering = false
    @State private var selectedItemID: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 12) {
                Text(question.title)
                    .font(.system(size: 17, weight: .semibold))
                Text(question.explanation)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let cause = question.recoveryCause {
                    Label(cause.text, systemImage: "clock")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .background(RoundedRectangle(cornerRadius: 8).fill(.quaternary.opacity(0.6)))
                }
                HStack {
                    Text(question.itemsHeading)
                        .fontWeight(.semibold)
                    Spacer()
                    Text("Asked \(Date(milliseconds: question.askedAtMs), format: .relative(presentation: .named))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.top, 4)
            }
            .padding([.horizontal, .top], 20)
            .padding(.bottom, 8)
            List(question.items, id: \.nodeId, selection: $selectedItemID) { item in
                HeldItemRow(item: item)
                    .help(item.path)
                    .contextMenu {
                        Button("Copy Path") { copy(fullPath(item)) }
                    }
            }
            .listStyle(.bordered(alternatesRowBackgrounds: false))
            .frame(minHeight: 80)
            .padding(.horizontal, 20)
            .onCopyCommand {
                selectedItem.map { [NSItemProvider(object: fullPath($0) as NSString)] } ?? []
            }
            if let selectedItem {
                HStack(alignment: .firstTextBaseline) {
                    Text("Selected path:")
                        .foregroundStyle(.secondary)
                    Text(verbatim: fullPath(selectedItem))
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .lineLimit(2)
                        .truncationMode(.middle)
                    Spacer()
                    Button("Copy Path") { copy(fullPath(selectedItem)) }
                        .buttonStyle(.plain)
                        .foregroundStyle(Color.koiText)
                }
                .font(.caption)
                .padding(.horizontal, 20)
                .padding(.top, 8)
            }
            Spacer(minLength: 12)
                .frame(maxHeight: 12)
            Divider()
            footer
        }
        .onAppear { selectedItemID = question.items.first?.nodeId }
        .sheet(item: $confirming) { answer in
            ConfirmSheet(
                title: question.confirmation(for: answer),
                message: question.confirmationMessage(for: answer),
                action: question.title(for: answer),
                confirm: { confirming = nil; send(answer) },
                cancel: { confirming = nil }
            )
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            if let error {
                Label(error.message, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(Color.statusRed)
                    .lineLimit(2)
            } else {
                Label(question.scope, systemImage: "questionmark.circle")
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if answering {
                ProgressView().controlSize(.small)
            }
            ForEach(orderedAnswers, id: \.self) { answer in
                if answer == primaryAnswer {
                    Button(question.title(for: answer)) { choose(answer) }
                        .buttonStyle(.prominent)
                } else {
                    Button(question.title(for: answer), role: answer.isDestructive ? .destructive : nil) { choose(answer) }
                        .foregroundStyle(answer.isDestructive ? Color.statusRed : .primary)
                }
            }
        }
        .disabled(answering)
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .background(.bar)
    }

    private var primaryAnswer: Answer? {
        question.answers.first { !$0.isDestructive }
    }

    private var orderedAnswers: [Answer] {
        question.answers.filter { $0 != primaryAnswer } + [primaryAnswer].compactMap { $0 }
    }

    private var selectedItem: HeldItem? {
        guard let selectedItemID else { return nil }
        return question.items.first { $0.nodeId == selectedItemID }
    }

    private func fullPath(_ item: HeldItem) -> String {
        guard let folder else { return item.path }
        return abbreviatingHome(URL(filePath: folder.path).appending(path: item.path).path(percentEncoded: false))
    }

    private func copy(_ path: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(path, forType: .string)
    }

    private func choose(_ answer: Answer) {
        if answer.isDestructive {
            confirming = answer
        } else {
            send(answer)
        }
    }

    private func send(_ answer: Answer) {
        answering = true
        error = nil
        Task {
            do {
                try await model.answer(question, with: answer)
            } catch let failure as SessionError {
                error = failure
            } catch {}
            answering = false
        }
    }
}

extension Answer: @retroactive Identifiable {
    public var id: Self { self }
}

struct ConfirmSheet: View {
    let title: String
    let message: String
    let action: String
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 32))
                .foregroundStyle(Color.statusRed)
            Text(title)
                .font(.headline)
                .multilineTextAlignment(.center)
            Text(message)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            VStack(spacing: 8) {
                Button(action: confirm) {
                    Text(action).frame(maxWidth: .infinity)
                }
                .buttonStyle(DestructiveFillButtonStyle())
                Button(action: cancel) {
                    Text("Cancel").frame(maxWidth: .infinity)
                }
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .keyboardShortcut(.cancelAction)
            }
            .padding(.top, 4)
        }
        .padding(20)
        .frame(width: 280)
    }
}

struct DestructiveFillButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.body.weight(.medium))
            .foregroundStyle(.white)
            .padding(.vertical, 5)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color.redFill.opacity(configuration.isPressed ? 0.8 : 1)))
    }
}

private struct HeldItemRow: View {
    let item: HeldItem

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: item.kind == .folder ? "folder" : "doc")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(verbatim: item.path)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let detail = item.detail.text {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            Text(verbatim: fileCount(item.files))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}

#Preview("Mass delete") {
    QuestionsView().environment(AppModel.preview(.massDelete))
}

#Preview("One linked space") {
    let model = AppModel.preview(.massDelete)
    try? model.session.unlinkFolder(spaceId: "space-photos")
    return QuestionsView().environment(model)
}

#Preview("Recovery") {
    QuestionsView().environment(AppModel.preview(.recovery))
}

#Preview("Two spaces: many questions") {
    QuestionsView().environment(AppModel.preview(.manyQuestions))
}

#Preview("None") {
    QuestionsView().environment(AppModel.preview(.upToDate))
}
