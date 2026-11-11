# SPDX-License-Identifier: Apache-2.0
#
# The engine mutation set, run by scripts/mutations.sh. Each mutation applies
# one perl substitution (perl -0pi -e) to one file. The last field names the
# tests that must catch it: unit runs the commit tests, prop runs the
# interleaving property up to three times, both runs the unit tests first, and
# devices runs the tests of several devices up to three times.
# shellcheck shell=bash disable=SC2034

unit=(cargo test -q --locked -p flarebox-engine --test commit)
property=(cargo test -q --locked -p flarebox-sim --test engine_sync)
devices=(cargo test -q --locked -p flarebox-sim --test devices)

C=core/engine/src/commit.rs
E=core/engine/src/effects.rs
L=core/engine/src/classify.rs

mutation agreed-batch $C 's/    \(needed, agreed\)\n\}/    let _ = agreed;\n    (needed, Vec::new())\n}/' prop
mutation merge-delete-wait $C 's/scratch\.merged\.extend\(held\);/let _ = held;/' both
mutation pending-name $C 's/if held\.iter\(\)\.any\(\|other\| moving\.contains\(other\)\)/if held.is_empty()/' both
mutation renamed-revert $C 's/if renamed \|\| \(done && follows\)/if done \&\& follows/' both
mutation merged-revert $C 's/trees\.revert\(create\.node_id\.clone\(\)\);\n//' both
mutation cycle-revert $C 's/        for id in reverts \{\n            trees\.revert\(id\);/        for id in reverts {\n            let _ = id;/' both
mutation split-halves $C 's/self\.cap = Some\(ops\.len\(\) \/ 2\);/self.cap = Some(ops.len());/' both
mutation split-delete $C 's/self\.split\.insert\(delete\.node_id\);/let _ = delete;/' unit
mutation cap-regrowth $C 's/self\.cap\.map\(\|cap\| cap \* 2\)/self.cap.map(|cap| cap)/' unit
mutation byte-budget $C 's/\(emitted > 0 && bytes \+ cost > MAX_COMMIT_BYTES\)/(emitted > 0 \&\& bytes + cost > usize::MAX - cost)/' unit
mutation large-delete $C 's/if bytes > MAX_COMMIT_BYTES \|\| split/if split/' unit
mutation device-seq $C 's/device_seq: self\.accepted \+ 1,/device_seq: self.accepted,/' unit
mutation accepted $C 's/        self\.accepted = flight\.commit\.device_seq;\n//' unit
mutation limit-split $C 's/Ok\(Code::LimitExceeded\) => Failure::Split,/Ok(Code::LimitExceeded) => Failure::Abandoned,/' both
mutation alias-plan $C 's/let local = match aliases\.get\(&local\.parent\)/let local = match None::<\&NodeId>/' both
mutation revert-no-rewrite $E 's/!same_content\(old, new\) && self\.trees\.class\(id\) == Class::Remote/!same_content(old, new)/' both
mutation reverted-candidate $E 's/\n        \|\| \(trees\.reverted\(\)\.contains\(id\)\n            && \(trees\.remote\(\)\.live\(id\)\.is_some\(\) \|\| !trees\.edited\(id\)\)\)//' both
mutation reverted-edit-guard $E 's/\n            && \(trees\.remote\(\)\.live\(id\)\.is_some\(\) \|\| !trees\.edited\(id\)\)\)/)/' both
mutation reverted-delete $C 's/if synced\.is_some\(\) && trees\.class\(id\) == Class::Local && !needed\.contains\(id\) \{/if synced.is_some() \&\& trees.class(id) == Class::Local \&\& !needed.contains(id) \&\& !trees.reverted().contains(id) {/' both
mutation reverted-no-create $C 's/if !trees\.reverted\(\)\.contains\(id\) \{/if true {/' both
mutation reverted-no-move $C 's/\n            && !trees\.reverted\(\)\.contains\(id\)\n        \{/\n        {/' both
mutation reverted-parking $E 's/if !candidate\(trees, &id\) \|\|/if trees.class(\&id) != Class::Remote ||/' both
mutation pending-truncate $C 's/while nfc\(&named\(&stem\)\)\.len\(\) > MAX_NAME_BYTES/while false \&\& nfc(\&named(\&stem)).len() > MAX_NAME_BYTES/' both
mutation keep-parked $C 's/\.is_some_and\(\|node\| node\.parked \|\| trees\.reverted\(\)\.contains\(id\)\);/.is_some_and(|_| trees.reverted().contains(id));/' both
mutation parked-reverted-remove $E 's/\n                    && \(!self\.trees\.reverted\(\)\.contains\(id\) \|\| self\.trees\.edited\(id\)\) =>/ =>/' both
mutation parked-reverted-edit $E 's/ \|\| self\.trees\.edited\(id\)\) =>/) =>/' both
mutation reverted-retain $L 's/\(local, remote\) => local\.is_some\(\) \|\| remote\.is_some\(\),/(local, _) => local.is_some(),/' both
mutation alias-in-place $C 's/            reverts\.push\(id\.clone\(\)\);\n        \}\n        if synced/            let _ = id;\n        }\n        if synced/' both
mutation rekey-edit $C 's/rekey\(trees, id, &ids\.uuid_v7\(now_ms\)\);/let _ = (\&ids, now_ms);/' both
mutation keep-revert-blocked $C 's/if !trees\.set_synced_if_valid\(current\.clone\(\), None\) \{\n                    trees\.revert\(current\.clone\(\)\);/if !trees.set_synced_if_valid(current.clone(), None) {/' both
mutation quota-retry $C 's/            Ok\(Code::QuotaExceeded\) if error\.retry_after_ms > 0 => Failure::Resend,\n//' unit
mutation refused-reconnect $C 's/        if self\.halted == Some\(Halt::Refused\) \{\n            return Reconnect::Idle;\n        \}\n//' unit
mutation unknown-resend $C 's/            _ => Failure::Halted\(Halt::Refused\),/            Err(_) => Failure::Resend,\n            _ => Failure::Halted(Halt::Refused),/' unit
mutation parked-name $L 's/self\.parked_as\.get\(id\)\.map_or\(id, String::as_str\)/{ let _ = \&self.parked_as; id }/' unit
mutation rekey-park-as $C 's/        trees\.park_as\(to\.to_owned\(\), parked_name\);\n//' unit
mutation digest-dedupe $E 's/    let changed = remote_only\(trees, id\)\n/    let changed = trees.local().get(id).is_some_and(|node| {\n        let digest = |node: \&LocalNode| node.shape.content.as_ref().map(|content| content.manifest_digest.clone());\n        let twin = |(other, twin): (\&NodeId, \&LocalNode)| other.as_str() < id \&\& digest(twin) == digest(node);\n        digest(node).is_some() \&\& trees.synced().live(id).is_none() \&\& trees.remote().live(id).is_none() \&\& trees.local().iter().any(twin)\n    }) || remote_only(trees, id)\n/' devices
