// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::tracked::{Marked, TrackedMap, TrackedSet};

fn set(keys: &[&str]) -> TrackedSet<String> {
    BTreeSet::from_iter(keys.iter().map(|key| key.to_string())).into()
}

fn map(entries: &[(&str, u64)]) -> TrackedMap<String, u64> {
    BTreeMap::from_iter(entries.iter().map(|(key, value)| (key.to_string(), *value))).into()
}

fn owned<V: Clone>(changes: &[(&str, V)]) -> Vec<(String, V)> {
    (changes.iter())
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

#[test]
fn a_collection_built_from_its_contents_has_no_changes() {
    assert_eq!(set(&["a", "b"]).take_changes(), []);
    assert_eq!(map(&[("a", 1)]).take_changes(), []);
    assert_eq!(Marked::from(1).take_changed(), None);
}

#[test]
fn each_set_mutator_reports_the_keys_whose_membership_changed() {
    let mut tracked = set(&["a", "b", "c", "d"]);
    assert!(tracked.insert("x".into()));
    assert!(!tracked.insert("a".into()));
    assert!(tracked.remove("b"));
    assert!(!tracked.remove("y"));
    tracked.retain(|key| key != "c");
    tracked.extend(["a".to_owned(), "z".to_owned()]);
    let expected = owned(&[("b", false), ("c", false), ("x", true), ("z", true)]);
    assert_eq!(tracked.take_changes(), expected);
    assert_eq!(tracked.take_changes(), [], "a take drains the changes");
    tracked.clear();
    let expected = owned(&[("a", false), ("d", false), ("x", false), ("z", false)]);
    assert_eq!(tracked.take_changes(), expected);
}

#[test]
fn a_set_change_undone_before_the_take_is_no_change() {
    let mut tracked = set(&["a"]);
    tracked.remove("a");
    tracked.insert("a".into());
    tracked.insert("b".into());
    tracked.remove("b");
    tracked.clear();
    tracked.extend(["a".to_owned()]);
    assert_eq!(tracked.take_changes(), []);
    assert_eq!(tracked, set(&["a"]));
}

#[test]
fn each_map_mutator_reports_the_keys_whose_value_changed_with_the_value_now() {
    let mut tracked = map(&[("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)]);
    assert_eq!(tracked.insert("a".into(), 1), Some(1));
    assert_eq!(tracked.insert("b".into(), 20), Some(2));
    assert_eq!(tracked.insert("x".into(), 9), None);
    assert_eq!(tracked.remove("c"), Some(3));
    assert_eq!(tracked.remove("y"), None);
    *tracked.get_mut("d").unwrap() += 40;
    assert!(tracked.get_mut("y").is_none());
    *tracked.get_or_insert_with("z".into(), || 7) += 1;
    *tracked.get_or_insert_with("e".into(), || 7) += 0;
    tracked.retain(|key, _| key != "x");
    let expected = owned(&[
        ("b", Some(20)),
        ("c", None),
        ("d", Some(44)),
        ("z", Some(8)),
    ]);
    assert_eq!(tracked.take_changes(), expected);
    assert_eq!(tracked.take_changes(), [], "a take drains the changes");
    tracked.extend([("a".to_owned(), 1), ("b".to_owned(), 21)]);
    assert_eq!(tracked.pop_first(), Some(("a".to_owned(), 1)));
    assert_eq!(
        tracked.take_changes(),
        owned(&[("a", None), ("b", Some(21))])
    );
    assert_eq!(tracked.take_all().len(), 4);
    let expected = owned(&[("b", None), ("d", None), ("e", None), ("z", None)]);
    assert_eq!(tracked.take_changes(), expected);
}

#[test]
fn a_map_change_undone_before_the_take_is_no_change() {
    let mut tracked = map(&[("a", 1)]);
    *tracked.get_mut("a").unwrap() = 2;
    *tracked.get_mut("a").unwrap() = 1;
    tracked.remove("a");
    tracked.insert("a".into(), 1);
    tracked.insert("b".into(), 1);
    tracked.remove("b");
    let all = tracked.take_all();
    tracked.extend(all);
    tracked.clear();
    tracked.insert("a".into(), 1);
    assert_eq!(tracked.take_changes(), []);
    assert_eq!(tracked, map(&[("a", 1)]));
}

#[test]
fn a_marked_value_reports_itself_when_it_differs_from_the_last_take() {
    let mut marked = Marked::from(vec![1]);
    marked.set(vec![1]);
    assert_eq!(marked.take_changed(), None);
    marked.get_mut().push(2);
    assert_eq!(marked.take_changed(), Some(&vec![1, 2]));
    assert_eq!(marked.take_changed(), None, "a take drains the change");
    marked.get_mut().pop();
    marked.set(vec![1, 2]);
    assert_eq!(marked.take_changed(), None);
    marked.set(vec![3]);
    assert_eq!(marked.take_changed(), Some(&vec![3]));
    let mut held = Marked::from(Some(1));
    assert_eq!(held.take_if(|value| *value == 2), None);
    assert_eq!(held.take_changed(), None);
    assert_eq!(held.take_if(|value| *value == 1), Some(1));
    assert_eq!(held.take_changed(), Some(&None));
}

#[test]
fn equality_and_debug_see_the_contents_only() {
    let mut touched = set(&["a"]);
    touched.insert("b".into());
    assert_eq!(touched, set(&["a", "b"]));
    assert_eq!(format!("{touched:?}"), r#"{"a", "b"}"#);
    let mut changed = map(&[("a", 1)]);
    *changed.get_mut("a").unwrap() = 2;
    assert_eq!(changed, map(&[("a", 2)]));
    assert_eq!(format!("{changed:?}"), r#"{"a": 2}"#);
    let mut marked = Marked::from(1);
    marked.set(2);
    assert_eq!(marked, Marked::from(2));
    assert_eq!(format!("{marked:?}"), "2");
}

#[test]
fn a_clone_keeps_the_changes_it_has_not_taken() {
    let mut tracked = set(&[]);
    tracked.insert("a".into());
    assert_eq!(tracked.clone().take_changes(), owned(&[("a", true)]));
    assert_eq!(tracked.take_changes(), owned(&[("a", true)]));
}
