// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_download_the_server_lost_is_sent_again_over_the_version_it_replaced() {
    let (mut commits, mut trees, mut ids) = downloaded_at_5();
    list_after_a_loss(
        &mut trees,
        &commits,
        version(remote(file(ROOT, "a", 1)), "v1"),
    );
    assert_eq!(trees.class("a"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [resend("a", 2)]);
}

#[test]
fn a_download_the_server_lost_is_not_sent_again_over_a_later_edit() {
    let (_, mut trees, _) = downloaded_at_5();
    let later = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..version(remote(file(ROOT, "a", 3)), "v3")
    };
    let commits = Commits::default();
    list_after_a_loss(&mut trees, &commits, later);
    assert_eq!(trees.class("a"), Class::Concurrent);
}

#[test]
fn a_download_the_server_lost_is_not_sent_again_over_a_restored_version() {
    let (commits, mut trees, _) = downloaded_at_5();
    let restored = RemoteNode {
        content_rev: 3,
        edit_rev: 3,
        ..version(remote(file(ROOT, "a", 1)), "v1-restored")
    };
    list_after_a_loss(&mut trees, &commits, restored);
    assert_eq!(trees.class("a"), Class::Concurrent);
}

#[test]
fn a_download_the_server_lost_is_not_sent_again_over_a_delete() {
    let (commits, mut trees, _) = downloaded_at_5();
    list_after_a_loss(
        &mut trees,
        &commits,
        version(trashed(file(ROOT, "a", 1)), "v1"),
    );
    assert_eq!(trees.class("a"), Class::Remote);
}

#[test]
fn a_download_the_server_lost_is_not_sent_again_at_another_position() {
    let (commits, mut trees, _) = downloaded_at_5();
    list_after_a_loss(
        &mut trees,
        &commits,
        version(remote(file(ROOT, "b", 1)), "v1"),
    );
    assert_eq!(trees.synced().get("a"), None);
}

#[test]
fn a_download_edited_since_is_not_listed_as_synced() {
    let (_, mut trees, _) = downloaded_at_5();
    trees.set_local("a".into(), local(file(ROOT, "a", 4), 1));
    trees.settle();
    assert!(
        !trees
            .taken_from_others(|_| false)
            .contains(&"a".to_string())
    );
}

#[test]
fn a_download_the_server_lost_is_not_sent_again_without_a_recorded_version() {
    let (commits, mut trees, _) = edited_at_5_then_lost();
    trees.set_local("b".into(), local(file(ROOT, "b", 1), 2));
    trees.set_remote("b".into(), remote(file(ROOT, "b", 1)));
    trees.settle();
    let edited = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote(file(ROOT, "b", 2))
    };
    trees.set_remote("b".into(), edited);
    trees.set_local("b".into(), local(file(ROOT, "b", 2), 2));
    trees.settle();
    assert_eq!(trees.class("b"), Class::InSync);
    let synced = trees.taken_from_others(|_| false);
    trees.set_remote("a".into(), remote(file(ROOT, "a", 1)));
    trees.set_remote("b".into(), remote(file(ROOT, "b", 1)));
    trees.forget_lost(
        Vec::new(),
        trees.edited_here(),
        synced,
        Vec::new(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("b"), Class::Concurrent);
}

#[test]
fn a_version_a_listing_took_r_back_from_is_not_sent_over() {
    let mut trees = trees(&[synced("a", file(ROOT, "a", 1))]);
    trees.set_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    let lost = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..version(remote(file(ROOT, "a", 2)), "v2")
    };
    trees.set_remote("a".into(), lost.clone());
    let commits = Commits::default();
    list_after_a_loss(
        &mut trees,
        &commits,
        version(remote(file(ROOT, "a", 1)), "v1"),
    );
    assert_eq!(trees.class("a"), Class::InSync);
    list_after_a_loss(&mut trees, &commits, lost);
    assert_eq!(trees.class("a"), Class::Concurrent);
    assert!(!trees.state().replaced_versions.contains_key("a"));
}

#[test]
fn a_listing_keeps_the_versions_recorded_up_to_the_one_it_lists() {
    let (_, mut trees, _) = downloaded_at_5();
    trees.set_remote("a".into(), version(remote(file(ROOT, "a", 3)), "v3"));
    assert_eq!(trees.state().replaced_versions["a"], ["v1", "v2"]);
    trees.set_listed_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    assert_eq!(trees.state().replaced_versions["a"], ["v1"]);
    trees.set_listed_remote("a".into(), version(remote(file(ROOT, "a", 1)), "v1"));
    assert_eq!(trees.state().replaced_versions["a"], ["v1"]);
}

#[test]
fn r_keeps_the_newest_versions_a_file_held() {
    let (_, mut trees, _) = downloaded_at_5();
    for n in 3..=(REPLACED_VERSIONS_KEPT as u8 + 3) {
        trees.set_remote(
            "a".into(),
            version(remote(file(ROOT, "a", n)), &format!("v{n}")),
        );
    }
    let kept = &trees.state().replaced_versions["a"];
    assert_eq!(kept.len(), REPLACED_VERSIONS_KEPT);
    assert_eq!(kept.first().map(String::as_str), Some("v3"));
    trees.remove_remote("a");
    assert!(trees.state().replaced_versions.is_empty());
}

#[test]
fn an_unsent_edit_with_the_bytes_of_a_download_the_server_lost_beats_a_later_delete() {
    let (commits, mut trees, _) = downloaded_at_5();
    let mut state = trees.state();
    state.unsent_edits.insert("a".into());
    state.dirty.insert("a".into());
    trees.restore(state).unwrap();
    trees.settle();
    list_after_a_loss(
        &mut trees,
        &commits,
        version(remote(file(ROOT, "a", 1)), "v1"),
    );
    let deleted = RemoteNode {
        meta_rev: 6,
        ..trashed(file(ROOT, "a", 1))
    };
    trees.set_remote("a".into(), deleted);
    trees.settle();
    assert_eq!(trees.unsent_edits().iter().collect::<Vec<_>>(), ["a"]);
    assert_eq!(trees.class("a"), Class::Local);
}

#[test]
fn a_lost_create_under_a_folder_deleted_after_the_loss_is_not_sent_again() {
    let mut trees = trees(&[synced("f", dir(ROOT, "f")), synced("c", file("f", "c", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let synced = trees.taken_from_others(|_| false);
    let deleted = RemoteNode {
        meta_rev: 3,
        ..trashed(dir(ROOT, "f"))
    };
    trees.set_listed_remote("f".into(), deleted);
    trees.remove_remote("c");
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        synced,
        Vec::new(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("c"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}
