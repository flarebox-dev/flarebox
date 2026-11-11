// SPDX-License-Identifier: Apache-2.0

//! Records where the process panicked, never the panic's message, which can hold a path.

use std::panic;

use crate::recorder::Pending;
use crate::schema::{Event, SourceFile};
use crate::store::Store;

/// Adds a hook that queues a [`Event::Panic`] and then runs the hook that was there before. It gives up instead of
/// waiting when the queue is busy.
pub fn install_panic_hook(store: Store) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if let Some(location) = info.location() {
            let event = Event::Panic {
                day: crate::today(),
                file: SourceFile::from_location(location.file()),
                line: location.line(),
            };
            let _ = store.try_absorb(Pending {
                events: vec![event],
                ..Pending::default()
            });
        }
        previous(info);
    }));
}
