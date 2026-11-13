// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_new_store_on_a_device_that_synced_the_space_before_goes_on_from_the_servers_count() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    drop(a);

    let again = Site::new(&remote, "a");
    let mut a = again.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["list", "subscribe", "stage"]);
    a.create(
        &mut remote,
        &again.root.join("more.txt"),
        b"from the folder linked again",
    );
    assert_eq!(a.sent[0].device_seq, 2);
    assert_eq!(remote.names(), ["more.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_rewound_space_stops_the_folder_at_welcome_with_the_seq_it_was_rewound_to() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    let welcome = Welcome {
        history_id: "rewound".into(),
        restore_seq: 7,
        restore_point_ms: 1_794_442_424_000,
        ..remote.space.welcome(&a.caller)
    };
    let stop = a.event(Event::Connected(welcome)).unwrap_err();
    assert_eq!(
        stop.to_string(),
        "the space was rewound on the server to seq 7 (requested 2026-11-12 00:13 UTC); this build cannot recover the folder yet"
    );
    assert_eq!(a.work.len(), 0);
}

#[test]
fn a_history_changed_resync_names_the_seq_even_when_it_is_0() {
    let mut remote = Remote::new();
    let (_site, mut a) = linked(&mut remote, "a");
    let stop = a
        .event(Event::ResyncRequired(ResyncRequired {
            reason: Reason::HistoryChanged as i32,
            restore_seq: 0,
            restore_point_ms: 1_794_442_424_000,
        }))
        .unwrap_err();
    assert!(
        stop.to_string().starts_with(
            "the space was rewound on the server to seq 0 (requested 2026-11-12 00:13 UTC)"
        ),
        "{stop}"
    );
}

#[test]
fn a_never_synced_store_takes_the_servers_count_only_at_its_first_welcome() {
    let remote = Remote::new();
    let site = Site::new(&remote, "a");
    let mut a = site.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 0);

    let welcome = Welcome {
        device_seq: 1,
        ..remote.space.welcome(&a.caller)
    };
    let stop = a.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(a.folder.engine.commits.accepted(), 0);
}

#[test]
fn a_store_that_took_the_servers_count_does_not_take_another() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    drop(a);

    let again = Site::new(&remote, "a");
    let mut a = again.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    assert_eq!(a.folder.engine.cursor, None);
    drop(a);

    let mut a = again.start();
    let welcome = Welcome {
        device_seq: 2,
        ..remote.space.welcome(&a.caller)
    };
    let stop = a.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(a.folder.engine.commits.accepted(), 1);
}

#[test]
fn a_synced_store_stops_for_recovery_when_the_server_counts_a_commit_it_does_not_hold() {
    let mut remote = Remote::new();
    // The old store read the space and committed nothing: it holds a cursor, and its count is 0 like a new store's.
    let (old, a) = linked(&mut remote, "a");
    assert!(a.folder.engine.cursor.is_some());
    drop(a);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    drop(a);

    let mut stale = old.start();
    let welcome = remote.space.welcome(&stale.caller);
    assert_eq!(welcome.device_seq, 1);
    let stop = stale.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(stale.folder.engine.commits.accepted(), 0);
    assert!(stale.work.is_empty());
}

#[test]
fn a_store_with_a_commit_in_flight_stops_for_recovery_when_the_server_counts_past_its_slot() {
    let mut remote = Remote::new();
    let (old, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    a.create(&mut remote, &new.root.join("again.txt"), b"and once more");
    drop(a);

    let mut stale = old.start();
    let welcome = remote.space.welcome(&stale.caller);
    assert_eq!(welcome.device_seq, 3);
    let stop = stale.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert!(stale.work.is_empty(), "the commit is not sent again");
    let commits = &stale.folder.engine.commits;
    assert_eq!(commits.accepted(), 1);
    assert_eq!(
        commits.in_flight().unwrap().commit.commit_id,
        commit.commit_id
    );
}

#[test]
fn a_commit_from_the_store_a_new_store_replaced_is_refused_by_the_server() {
    let mut remote = Remote::new();
    let (old, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    assert_eq!(a.sent[0].device_seq, commit.device_seq);
    drop(a);

    // The server counts one commit more than the old store acknowledged, which is what a lost response looks like,
    // so the old store sends its commit again. The slot is the new store's by now.
    let mut stale = old.start();
    stale.connect(&remote);
    let Some(Work::Send(again)) = stale.work.pop_front() else {
        panic!("the stored commit is sent again");
    };
    assert_eq!(again.commit_id, commit.commit_id);
    let refusal = remote.commit(&stale.caller, &again).unwrap_err();
    assert!(
        matches!(&refusal, CallError::Server(error) if error.code() == Code::StaleDeviceState),
        "{refusal}"
    );
    let step = stale.folder.sent(Err(refusal));
    let stop = stale.take(step).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(
        stale.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery)
    );
    assert_eq!(remote.space.head_seq(), 2);
    assert_eq!(remote.names(), ["more.txt", "notes.txt"]);
}
