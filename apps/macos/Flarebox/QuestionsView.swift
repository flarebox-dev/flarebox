// SPDX-License-Identifier: Apache-2.0

import FlareboxFFI
import FlareboxModel
import SwiftUI

/// Every unanswered question, grouped by folder, with every path each one affects and its two
/// answers. The engine keeps questions until they are answered, so this window lists them again
/// after a relaunch.
struct QuestionsView: View {
    @Environment(AppModel.self) private var model
    @State private var selection: String?

    var body: some View {
        NavigationSplitView {
            List(selection: $selection) {
                ForEach(model.questionSections) { section in
                    Section(section.folder?.spaceName ?? section.spaceID) {
                        if let cause = section.recoveryCause {
                            Label(cause.text, systemImage: "clock.arrow.circlepath")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .selectionDisabled()
                        }
                        ForEach(section.questions) { question in
                            QuestionRow(question: question)
                                .tag(question.id)
                        }
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 240, ideal: 280)
        } detail: {
            if let question = model.state.questions.first(where: { $0.id == selection }) {
                QuestionDetail(question: question, folder: model.folder(spaceID: question.spaceId))
                    .id(question.id)
            } else {
                ContentUnavailableView(
                    "No Questions",
                    systemImage: "checkmark.circle",
                    description: Text("Flarebox asks here when it needs a decision, such as before deleting many files.")
                )
            }
        }
        .navigationTitle("Questions")
        .onAppear(perform: seen)
        .onChange(of: model.unseenQuestionIDs) { seen() }
        .onChange(of: model.state.questions.map(\.id), initial: true) { _, ids in
            if selection == nil || !ids.contains(selection!) {
                selection = model.questionSections.first?.questions.first?.id
            }
        }
    }

    private func seen() {
        model.markQuestionsSeen()
    }
}

private struct QuestionRow: View {
    let question: Question

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(question.title)
            Text(verbatim: "\(itemCount(question.items.count)) · \(fileCount(question.affectedFiles))")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

private struct QuestionDetail: View {
    @Environment(AppModel.self) private var model
    let question: Question
    let folder: LinkedFolder?
    @State private var confirming: Answer?
    @State private var isConfirming = false
    @State private var error: SessionError?
    @State private var answering = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text(question.title)
                    .font(.title2.weight(.semibold))
                Text(question.explanation)
                    .fixedSize(horizontal: false, vertical: true)
                if let cause = question.recoveryCause {
                    Label(cause.text, systemImage: "clock.arrow.circlepath")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Text(verbatim: ([String(localized: "Asked \(Date(milliseconds: question.askedAtMs).formatted(.relative(presentation: .named)))"), itemCount(question.items.count), fileCount(question.affectedFiles)] + [folder?.path].compactMap { $0 }).joined(separator: " · "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack {
                    ForEach(question.answers, id: \.self) { answer in
                        Button(question.title(for: answer), role: answer.isDestructive ? .destructive : nil) {
                            if answer.isDestructive {
                                confirming = answer
                                isConfirming = true
                            } else {
                                send(answer)
                            }
                        }
                    }
                    if answering {
                        ProgressView().controlSize(.small)
                    }
                }
                .disabled(answering)
                if let error {
                    Label(error.message, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                }
            }
            .padding(20)
            Divider()
            List(question.items, id: \.nodeId) { item in
                HeldItemRow(item: item)
            }
            .listStyle(.inset)
        }
        .confirmationDialog(
            confirming.map { question.title(for: $0) } ?? "",
            isPresented: $isConfirming,
            presenting: confirming
        ) { answer in
            Button(question.title(for: answer), role: .destructive) { send(answer) }
        } message: { _ in
            Text("This applies to every item listed: \(itemCount(question.items.count)), \(fileCount(question.affectedFiles)).")
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

private struct HeldItemRow: View {
    let item: HeldItem

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: item.kind == .folder ? "folder" : "doc")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(item.path)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let detail = item.detail.text {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            if item.kind == .folder {
                Text(verbatim: fileCount(item.files))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

#Preview("Mass delete") {
    QuestionsView().environment(AppModel.preview(.massDelete))
}

#Preview("Recovery") {
    QuestionsView().environment(AppModel.preview(.recovery))
}

#Preview("Many questions") {
    QuestionsView().environment(AppModel.preview(.manyQuestions))
}

#Preview("None") {
    QuestionsView().environment(AppModel.preview(.upToDate))
}
