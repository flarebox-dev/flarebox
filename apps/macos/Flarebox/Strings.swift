// SPDX-License-Identifier: Apache-2.0

import FlareboxFFI
import Foundation

// User-facing text for the core's values. Every string goes through the Localizable catalog.

extension SyncStatus {
    var title: String {
        switch self {
        case .stopped: String(localized: "Sync is stopped")
        case .needsSetup: String(localized: "Finish setting up Flarebox")
        case .connecting: String(localized: "Connecting…")
        case .syncing: String(localized: "Syncing…")
        case .idle: String(localized: "Up to date")
        case .paused: String(localized: "Sync is paused")
        case .offline: String(localized: "Can't reach the server. Retrying…")
        }
    }

    var symbolName: String {
        switch self {
        case .stopped, .needsSetup: "arrow.triangle.2.circlepath.circle"
        case .connecting, .syncing, .idle: "arrow.triangle.2.circlepath.circle.fill"
        case .paused: "pause.circle"
        case .offline: "bolt.horizontal.circle"
        }
    }
}

extension FolderState {
    var title: String {
        switch self {
        case .waiting: String(localized: "Waiting")
        case .syncing: String(localized: "Syncing")
        case .upToDate: String(localized: "Up to date")
        case .halted: String(localized: "Stopped by a problem")
        case .recovering: String(localized: "Recovering")
        case .readOnly: String(localized: "Download only")
        }
    }
}

extension Question {
    var title: String {
        switch kind {
        case .massDelete: String(localized: "Delete many files?")
        case .ambiguousDisappearance: String(localized: "Files disappeared")
        case .recovery(_, let group): group.title
        }
    }

    var explanation: String {
        switch kind {
        case .massDelete(let deletions, let affectedFiles, let spaceFiles, let exceeded):
            let limits = exceeded.map { limit in
                switch limit {
                case .deletions(let limit):
                    String(localized: "more than \(limit.formatted()) deletions in 10 minutes")
                case .shareOfSpace(let percent):
                    String(localized: "more than \(percent)% of the folder's files in 10 minutes")
                }
            }.formatted(.list(type: .or))
            return String(localized: "Flarebox is holding these deletions instead of sending them, because they would make \(limits): \(deletionCount(deletions)) removing \(fileCount(affectedFiles)) from a folder of \(fileCount(spaceFiles)). Other changes keep syncing.")
        case .ambiguousDisappearance:
            return String(localized: "These files disappeared while Flarebox was not tracking changes. They may have been moved into an ignored folder rather than deleted, so they stay on the server and your other devices until you decide.")
        case .recovery(_, let group):
            return group.explanation
        }
    }

    /// The title of an answer's button, which depends on the question.
    func title(for answer: Answer) -> String {
        switch (answer, kind) {
        case (.delete, .recovery): String(localized: "Delete on Server")
        case (.delete, _): String(localized: "Delete Everywhere")
        case (.restore, _): String(localized: "Restore Here")
        case (.keep, _): String(localized: "Put Back on Server")
        case (.discard, _): String(localized: "Move to Local Trash")
        case (.publish, _): String(localized: "Upload This Version")
        case (.repeat, _): String(localized: "Repeat Request")
        case (.cancel, _): String(localized: "Cancel Request")
        }
    }
}

extension RecoveryGroup {
    var title: String {
        switch self {
        case .createdAfterRestorePoint: String(localized: "Created after the restore point")
        case .changedOnThisDevice: String(localized: "Changed on this Mac")
        case .createdOnThisDevice: String(localized: "Created on this Mac")
        case .deletedOnThisDevice: String(localized: "Deleted on this Mac")
        case .requestedBeforeReset: String(localized: "Requested before the reset")
        }
    }

    var explanation: String {
        switch self {
        case .createdAfterRestorePoint:
            String(localized: "These files were created after the point the folder was restored to, so the restore removed them from the server. This Mac still has them.")
        case .changedOnThisDevice:
            String(localized: "These files were edited or moved on this Mac since it last synced. Upload this Mac's versions, or move them to the local trash and take the server's.")
        case .createdOnThisDevice:
            String(localized: "These files exist only on this Mac. Upload them, or move them to the local trash.")
        case .deletedOnThisDevice:
            String(localized: "These files were deleted on this Mac but are still on the server. Delete them on the server too, or download them again.")
        case .requestedBeforeReset:
            String(localized: "These restores were requested before the reset, and the server does not show them. Send them again, or drop them.")
        }
    }
}

extension RecoveryCause {
    var text: String {
        let cause = switch trigger {
        case .spaceRewound: String(localized: "This folder was restored to an earlier point on the server.")
        case .storeRolledBack: String(localized: "This Mac's sync state is older than its history on the server, for example after restoring a backup.")
        case .storeOlderThanDisk: String(localized: "This Mac's sync state is older than the files in the folder.")
        case .spaceImported: String(localized: "This folder was linked to a space imported from an export.")
        case .noSyncState: String(localized: "This folder already had files when it was linked, so Flarebox could not match them all with certainty.")
        }
        guard let ms = restorePointMs else { return cause }
        let date = Date(milliseconds: ms).formatted(date: .abbreviated, time: .shortened)
        return cause + " " + String(localized: "Restore point: \(date).")
    }
}

extension Notice {
    var title: String {
        switch kind {
        case .recovery: String(localized: "Folder recovered")
        case .remoteMassDelete: String(localized: "Many files were deleted on another device")
        }
    }

    var text: String {
        switch kind {
        case .recovery(let cause, let heldFiles):
            let held = heldFiles == 0
                ? String(localized: "Nothing needed your decision.")
                : String(localized: "\(fileCount(heldFiles)) wait for your answers in Questions.")
            return cause.text + " " + String(localized: "Flarebox applied what this Mac had not changed and moved the files it removed to the local trash.") + " " + held
        case .remoteMassDelete(let deletions, let affectedFiles, let spaceFiles, _):
            return String(localized: "\(deletionCount(deletions)) on another device removed \(fileCount(affectedFiles)) of \(fileCount(spaceFiles)) in this folder. The server decides, so Flarebox applied them here; the files are in the folder's local trash.")
        }
    }
}

extension ItemDetail {
    var text: String? {
        switch self {
        case .plain: nil
        case .edited: String(localized: "Edited")
        case .moved(let from): String(localized: "Moved from \(from)")
        case .editedAndMoved(let from): String(localized: "Edited and moved from \(from)")
        case .restoreFromTrash: String(localized: "Restore from trash")
        case .restoreVersion(let ms):
            String(localized: "Restore the version from \(Date(milliseconds: ms).formatted(date: .abbreviated, time: .shortened))")
        }
    }
}

extension ProblemKind {
    var title: String {
        switch self {
        case .folderMissing: String(localized: "The folder is missing")
        case .folderNotAFolder: String(localized: "The linked path is not a folder")
        case .folderUnreadable: String(localized: "The folder can't be read")
        case .folderOnOtherVolume: String(localized: "A different disk is at the folder's location")
        case .markerMissing: String(localized: "The folder's Flarebox marker is missing")
        case .markerUnreadable: String(localized: "The folder's Flarebox marker can't be read")
        case .markerOfOtherSpace: String(localized: "The folder belongs to another space")
        case .diskOlderThanSyncState: String(localized: "The folder was restored from an older backup")
        case .unsupportedFileSystem: String(localized: "The folder's file system isn't supported")
        case .spaceFormatTooNew: String(localized: "This space needs a newer Flarebox")
        case .fileUnreadable: String(localized: "A file can't be read")
        case .fileNotWritable: String(localized: "A file can't be written")
        case .fileBusy: String(localized: "A file keeps changing")
        case .nameNotAllowed: String(localized: "A name isn't allowed on this Mac")
        case .nameTaken: String(localized: "A name is taken by an ignored item")
        case .changeRejected: String(localized: "The server rejected a change")
        case .ignoreRuleInvalid: String(localized: "An ignore rule can't be read")
        case .signInRequired: String(localized: "Sign in again")
        case .stateNotWritable: String(localized: "Flarebox can't save its sync state")
        }
    }

    var advice: String {
        switch self {
        case .folderMissing:
            String(localized: "Sync for this folder is stopped. If it is on an external disk, connect the disk. Otherwise, put the folder back or unlink it.")
        case .folderNotAFolder:
            String(localized: "Sync for this folder is stopped. Put the folder back at its path or unlink it.")
        case .folderUnreadable(let reason):
            String(localized: "Sync for this folder is stopped until Flarebox can read it: \(reason)")
        case .folderOnOtherVolume:
            String(localized: "Sync for this folder is stopped so that files on the other disk aren't taken as changes. Mount the original disk.")
        case .markerMissing:
            String(localized: "Sync for this folder is stopped, because without the .flarebox folder it can't tell an empty folder from a deleted one. Put .flarebox back or unlink the folder.")
        case .markerUnreadable(let reason):
            String(localized: "Sync for this folder is stopped: \(reason)")
        case .markerOfOtherSpace(let spaceID):
            String(localized: "Sync for this folder is stopped, because its marker names the space \(spaceID). Unlink the folder and link the right one.")
        case .diskOlderThanSyncState:
            String(localized: "Sync for this folder is stopped so that older files aren't uploaded as edits.")
        case .unsupportedFileSystem:
            String(localized: "Flarebox can only apply changes safely where the file system can rename without replacing. Move the folder to your Mac's disk.")
        case .spaceFormatTooNew:
            String(localized: "Downloads continue, but nothing is uploaded until you update Flarebox.")
        case .fileUnreadable(let reason):
            String(localized: "It isn't uploaded until Flarebox can read it: \(reason)")
        case .fileNotWritable(let reason):
            String(localized: "A change from the server can't be applied to it: \(reason)")
        case .fileBusy:
            String(localized: "It changes too often to be uploaded, as a log or a database being written does. Flarebox keeps retrying; consider ignoring it in .fbignore.")
        case .nameNotAllowed:
            String(localized: "A file from the server has a name this Mac's file system can't hold. Rename it on another device.")
        case .nameTaken:
            String(localized: "A file from the server can't be placed because an ignored or unsupported item, such as a symbolic link, already has its name. Rename or move that item.")
        case .changeRejected(let reason):
            String(localized: "The server gave this reason: \(reason)")
        case .ignoreRuleInvalid(let line):
            String(localized: "Line \(line) of this .fbignore file is ignored. Fix or remove it.")
        case .signInRequired:
            String(localized: "The server no longer accepts this Mac. Sign out in the Account settings and sign in again.")
        case .stateNotWritable(let reason):
            String(localized: "Nothing syncs until Flarebox can save again: \(reason). Check that your disk has free space.")
        }
    }
}

extension RecentChange {
    var summary: String {
        let way = direction == .uploaded ? String(localized: "from this Mac") : String(localized: "from another device")
        return switch change {
        case .added: String(localized: "Added \(way)")
        case .edited: String(localized: "Edited \(way)")
        case .moved(let from): String(localized: "Moved from \(from) \(way)")
        case .deleted: String(localized: "Deleted \(way)")
        case .conflictCopy(let original): String(localized: "Conflicting copy of \(original)")
        }
    }
}

extension SessionError {
    var message: String {
        switch self {
        case .InvalidServerUrl: String(localized: "Enter the server's address, starting with https://.")
        case .ServerUnreachable(let reason): String(localized: "Can't reach the server: \(reason)")
        case .ServerIncompatible(let version): String(localized: "The server runs version \(version), which this version of Flarebox can't use.")
        case .NoServer: String(localized: "Connect to a server first.")
        case .AlreadyClaimed: String(localized: "This server already has an owner. Sign in with a pairing code instead.")
        case .NotClaimed: String(localized: "This server has no owner yet. Claim it with the code from its log.")
        case .InvalidCode: String(localized: "The code is wrong or has expired.")
        case .TooManyAttempts: String(localized: "Too many wrong codes. Try again in a few minutes.")
        case .EmptyName: String(localized: "Enter a name.")
        case .NotSignedIn: String(localized: "This Mac isn't signed in.")
        case .SpaceNotFound: String(localized: "The space no longer exists.")
        case .FolderNotFound: String(localized: "Choose an existing folder.")
        case .SpaceAlreadyLinked: String(localized: "This space is already linked to a folder on this Mac.")
        case .FolderOverlaps(let other): String(localized: "This folder contains, or is inside, the linked folder \(other).")
        case .FolderNotLinked: String(localized: "This space isn't linked on this Mac.")
        case .QuestionNotFound: String(localized: "This question was already answered or has changed.")
        case .AnswerNotAllowed: String(localized: "This question doesn't take that answer.")
        case .NoticeNotFound: String(localized: "This notice was already dismissed.")
        case .NotSaved(let reason): String(localized: "Flarebox couldn't save this change, so it wasn't made: \(reason)")
        }
    }
}

/// "1 item", "2 items", with plural forms from the catalog.
func itemCount(_ count: Int) -> String {
    String(localized: "\(count) items")
}

/// "1 deletion", "2 deletions", with plural forms from the catalog.
func deletionCount(_ count: UInt64) -> String {
    String(localized: "\(count) deletions")
}

/// "1 file", "2 files", with plural forms from the catalog.
func fileCount(_ count: UInt64) -> String {
    String(localized: "\(count) files")
}

extension Date {
    init(milliseconds: Int64) {
        self.init(timeIntervalSince1970: TimeInterval(milliseconds) / 1000)
    }
}

extension FakeScenario {
    /// Reads the scenario named by the `FakeScenario` launch argument, such as `massDelete`.
    init?(name: String) {
        let all: [String: FakeScenario] = [
            "firstRun": .firstRun, "upToDate": .upToDate, "syncing": .syncing, "paused": .paused,
            "offline": .offline, "massDelete": .massDelete, "ambiguousDisappearance": .ambiguousDisappearance,
            "recovery": .recovery, "recoveredWithoutQuestions": .recoveredWithoutQuestions,
            "remoteMassDelete": .remoteMassDelete, "manyQuestions": .manyQuestions, "problems": .problems,
        ]
        guard let scenario = all[name] else { return nil }
        self = scenario
    }
}

let independenceNotice = String(localized: "Flarebox is an independent project and is not affiliated with Cloudflare, Inc.")
