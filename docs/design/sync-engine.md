# Sync engine design

[Design index](sync-engine/README.md)

## Summary

[Read section](sync-engine/summary.md#summary)

## Changes from the approved draft

[Read section](sync-engine/approved-draft-changes.md#changes-from-the-approved-draft)

## 1. Context

[Read section](sync-engine/context.md#1-context)

## 2. State model

[Read section](sync-engine/state-model.md#2-state-model)

### 2.1 Options

[Read section](sync-engine/state-model.md#21-options)

### 2.2 The trees

[Read section](sync-engine/state-model.md#22-the-trees)

### 2.3 Classifying a node

[Read section](sync-engine/state-model.md#23-classifying-a-node)

### 2.4 Concurrent changes the client resolves

[Read section](sync-engine/state-model.md#24-concurrent-changes-the-client-resolves)

### 2.5 Applying remote changes to disk

[Read section](sync-engine/state-model.md#25-applying-remote-changes-to-disk)

### 2.6 Building a commit

[Read section](sync-engine/state-model.md#26-building-a-commit)

### 2.7 After a commit result

[Read section](sync-engine/state-model.md#27-after-a-commit-result)

### 2.8 Persistence and crash recovery

[Read section](sync-engine/state-model.md#28-persistence-and-crash-recovery)

## 3. Correctness properties

[Read section](sync-engine/correctness.md#3-correctness-properties)

### 3.1 Vocabulary

[Read section](sync-engine/correctness.md#31-vocabulary)

### 3.2 Safety

[Read section](sync-engine/correctness.md#32-safety)

### 3.3 No data loss

[Read section](sync-engine/correctness.md#33-no-data-loss)

### 3.4 Convergence and termination

[Read section](sync-engine/correctness.md#34-convergence-and-termination)

### 3.5 No resurrection

[Read section](sync-engine/correctness.md#35-no-resurrection)

### 3.6 Moves

[Read section](sync-engine/correctness.md#36-moves)

## 4. The server's total order

[Read section](sync-engine/server-order.md#4-the-servers-total-order)

### 4.1 Commit

[Read section](sync-engine/server-order.md#41-commit)

### 4.2 Errors and retries

[Read section](sync-engine/server-order.md#42-errors-and-retries)

### 4.3 Rebase

[Read section](sync-engine/server-order.md#43-rebase)

### 4.4 Receiving changes

[Read section](sync-engine/server-order.md#44-receiving-changes)

### 4.5 What is not a CRDT

[Read section](sync-engine/server-order.md#45-what-is-not-a-crdt)

## 5. Local file system edge cases

[Read section](sync-engine/filesystem.md#5-local-file-system-edge-cases)

### 5.1 Lossy watcher events and rescans

[Read section](sync-engine/filesystem.md#51-lossy-watcher-events-and-rescans)

### 5.2 Applying effects without losing user bytes

[Read section](sync-engine/filesystem.md#52-applying-effects-without-losing-user-bytes)

### 5.3 Recovery, partial downloads and uploads

[Read section](sync-engine/filesystem.md#53-recovery-partial-downloads-and-uploads)

### 5.4 Case sensitivity

[Read section](sync-engine/filesystem.md#54-case-sensitivity)

### 5.5 Unicode normalization

[Read section](sync-engine/filesystem.md#55-unicode-normalization)

### 5.6 Symbolic links, special files and hard links

[Read section](sync-engine/filesystem.md#56-symbolic-links-special-files-and-hard-links)

### 5.7 Permissions and the executable bit

[Read section](sync-engine/filesystem.md#57-permissions-and-the-executable-bit)

### 5.8 Files that change while being chunked or uploaded

[Read section](sync-engine/filesystem.md#58-files-that-change-while-being-chunked-or-uploaded)

### 5.9 Very large directories and trees

[Read section](sync-engine/filesystem.md#59-very-large-directories-and-trees)

### 5.10 `.koiignore`

[Read section](sync-engine/filesystem.md#510-koiignore)

## 6. Mass-deletion safety

[Read section](sync-engine/mass-deletion.md#6-mass-deletion-safety)

### 6.1 Mass-delete guard

[Read section](sync-engine/mass-deletion.md#61-mass-delete-guard)

### 6.2 Missing-root detection

[Read section](sync-engine/mass-deletion.md#62-missing-root-detection)

### 6.3 Restoring after a bad mass delete

[Read section](sync-engine/mass-deletion.md#63-restoring-after-a-bad-mass-delete)

### 6.4 Recovery after a rewind or a rolled-back store

[Read section](sync-engine/mass-deletion.md#64-recovery-after-a-rewind-or-a-rolled-back-store)

## 7. Testing

[Read section](sync-engine/testing.md#7-testing)

### 7.1 Reference model

[Read section](sync-engine/testing.md#71-reference-model)

### 7.2 Property tests

[Read section](sync-engine/testing.md#72-property-tests)

### 7.3 Deterministic simulation

[Read section](sync-engine/testing.md#73-deterministic-simulation)

### 7.4 Native contract tests

[Read section](sync-engine/testing.md#74-native-contract-tests)

### 7.5 Named scenarios

[Read section](sync-engine/testing.md#75-named-scenarios)

## 8. Is a TLA+ model worth it?

[Read section](sync-engine/formal-model.md#8-is-a-tla-model-worth-it)

## 9. Interfaces with other tracks

[Read section](sync-engine/interfaces.md#9-interfaces-with-other-tracks)

## 10. Spec changes and questions

[Read section](sync-engine/spec-questions.md#10-spec-changes-and-questions)

## References

[Read section](sync-engine/references.md#references)

<a id="ref-bp"></a>[ref-bp](sync-engine/references.md#ref-bp)

<a id="ref-unison"></a>[ref-unison](sync-engine/references.md#ref-unison)

<a id="ref-move"></a>[ref-move](sync-engine/references.md#ref-move)

<a id="ref-nucleus"></a>[ref-nucleus](sync-engine/references.md#ref-nucleus)

<a id="ref-trinity"></a>[ref-trinity](sync-engine/references.md#ref-trinity)

<a id="ref-fdb"></a>[ref-fdb](sync-engine/references.md#ref-fdb)

<a id="ref-aws"></a>[ref-aws](sync-engine/references.md#ref-aws)

<a id="ref-stateright"></a>[ref-stateright](sync-engine/references.md#ref-stateright)

<a id="ref-pillai"></a>[ref-pillai](sync-engine/references.md#ref-pillai)

<a id="ref-inotify"></a>[ref-inotify](sync-engine/references.md#ref-inotify)

<a id="ref-fsevents"></a>[ref-fsevents](sync-engine/references.md#ref-fsevents)

<a id="ref-rename"></a>[ref-rename](sync-engine/references.md#ref-rename)

<a id="ref-openat2"></a>[ref-openat2](sync-engine/references.md#ref-openat2)

<a id="ref-fsync-apple"></a>[ref-fsync-apple](sync-engine/references.md#ref-fsync-apple)

<a id="ref-gitignore"></a>[ref-gitignore](sync-engine/references.md#ref-gitignore)

<a id="ref-renamex"></a>[ref-renamex](sync-engine/references.md#ref-renamex)

<a id="ref-apfs"></a>[ref-apfs](sync-engine/references.md#ref-apfs)

<a id="ref-casefold"></a>[ref-casefold](sync-engine/references.md#ref-casefold)

<a id="ref-unison-manual"></a>[ref-unison-manual](sync-engine/references.md#ref-unison-manual)

<a id="ref-rclone"></a>[ref-rclone](sync-engine/references.md#ref-rclone)

<a id="ref-onedrive"></a>[ref-onedrive](sync-engine/references.md#ref-onedrive)

<a id="ref-syncthing"></a>[ref-syncthing](sync-engine/references.md#ref-syncthing)

<a id="ref-syncthing-ignore"></a>[ref-syncthing-ignore](sync-engine/references.md#ref-syncthing-ignore)

<a id="ref-ds-store"></a>[ref-ds-store](sync-engine/references.md#ref-ds-store)

<a id="ref-pitr"></a>[ref-pitr](sync-engine/references.md#ref-pitr)
