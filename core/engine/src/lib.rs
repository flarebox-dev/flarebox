// SPDX-License-Identifier: Apache-2.0

//! Sync state machine and conflict rules, free of I/O. So far this holds the spec's identifier and
//! name rules, which the reference server in `flarebox-sim` shares.

mod casefold_data;
pub mod ids;
pub mod names;
