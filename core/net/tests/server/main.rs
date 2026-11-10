// SPDX-License-Identifier: Apache-2.0

//! The client against the real Worker in `server/`, run by `core/net/scripts/integration-test.sh`. One binary, so
//! every test shares the deployment's owner, which can be claimed once.

mod chunks;
mod devices;
mod session;
mod support;
