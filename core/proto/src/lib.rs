// SPDX-License-Identifier: Apache-2.0

//! Protocol types generated from `spec/proto`. The code in `src/gen/` is
//! written by `buf generate` in `spec/`; do not edit it by hand.

#[allow(clippy::all)]
pub mod v1 {
    include!("gen/flarebox/v1/flarebox.v1.rs");
}
