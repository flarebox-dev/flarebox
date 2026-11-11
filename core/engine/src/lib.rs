// SPDX-License-Identifier: Apache-2.0

//! Sync state machine and conflict rules, free of I/O (docs/design/sync-engine.md). So far this
//! holds the spec's identifier and name rules, which the reference server in `flarebox-sim` shares,
//! the three trees with the classification the planner starts from, the ignore rules that
//! exclude nodes on this device, the disk effects that apply remote changes, the commits that
//! upload local ones with the guard that holds mass deletions, the restores a person asks for, and the state the store keeps between runs.

mod casefold_data;
pub mod classify;
pub mod commit;
pub mod effects;
pub mod first_link;
pub mod guard;
pub mod ids;
pub mod ignore;
pub mod names;
pub mod question;
pub mod restore;
pub mod state;
pub mod tree;
