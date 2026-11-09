// SPDX-License-Identifier: Apache-2.0

//! Properties of the spec v0 section 6.1 name rules.

use flarebox_engine::names::{MAX_NAME_BYTES, casefold, conflict_name, name_key, normalize_name};
use proptest::prelude::*;

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Zß.]{1,12}",
        "\\PC{1,40}",
        "[가-힣]{80,90}",
        "[a-z]{240,255}(\\.[a-z]{1,8})?",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn name_key_is_idempotent(s in "\\PC{0,40}") {
        prop_assert_eq!(name_key(&name_key(&s)), name_key(&s));
        prop_assert_eq!(casefold(&casefold(&s)), casefold(&s));
    }

    #[test]
    fn normalizing_is_idempotent(s in name()) {
        if let Some(n) = normalize_name(&s) {
            prop_assert_eq!(normalize_name(&n), Some(n.clone()));
            prop_assert!(n.len() <= MAX_NAME_BYTES);
        }
    }

    #[test]
    fn conflict_names_are_valid_and_distinct(s in name(), device in "\\PC{0,40}", attempt in 1u32..5, day in 0i64..40_000) {
        let Some(original) = normalize_name(&s) else { return Ok(()) };
        let renamed = conflict_name(&original, &device, day * 86_400_000, attempt);
        prop_assert!(renamed.len() <= MAX_NAME_BYTES);
        prop_assert_eq!(normalize_name(&renamed), Some(renamed.clone()));
        prop_assert_ne!(name_key(&renamed), name_key(&original));
        prop_assert!(renamed.contains(" (conflict from "));
        if attempt > 1 {
            prop_assert_ne!(name_key(&renamed), name_key(&conflict_name(&original, &device, day * 86_400_000, attempt - 1)));
        }
    }
}
