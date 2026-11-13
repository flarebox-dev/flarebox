// SPDX-License-Identifier: Apache-2.0

//! Scans and watchers that apply the rules of design §5.10, with the matcher
//! of `koidrive-ignore` over the engine's name key.

include!("ignore/common.rs");
include!("ignore/deferred_searches.rs");
include!("ignore/hard_links.rs");
include!("ignore/moves_and_removals.rs");
include!("ignore/paused_searches.rs");
include!("ignore/rule_changes.rs");
include!("ignore/unknown_rules.rs");
include!("ignore/watchers.rs");
