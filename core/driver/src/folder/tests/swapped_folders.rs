// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_file_whose_folder_was_swapped_for_another_before_its_upload_is_uploaded() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let photos = site.root.join("photos");
    fs::create_dir(&photos).unwrap();
    fs::write(photos.join("one.jpeg"), b"one").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    fs::write(photos.join("one.jpeg"), b"one, edited").unwrap();
    fs::write(site.root.join("notes.txt"), b"notes").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    swap_for_a_new_folder(&photos);
    a.rest(&mut remote);

    let tree = remote.tree();
    assert_eq!(server_size(&remote, &tree["photos/one.jpeg"]), 11);
    assert!(tree.contains_key("notes.txt"));
    assert!(a.folder.engine.trees.dirty().is_empty());
    assert_eq!(a.folder.view().problem, None);
}

#[test]
fn a_folder_swapped_for_another_while_the_folder_was_stopped_takes_its_new_identity() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let photos = site.root.join("photos");
    fs::create_dir(&photos).unwrap();
    fs::write(photos.join("one.jpeg"), b"one").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    drop(a);

    swap_for_a_new_folder(&photos);
    fs::write(photos.join("one.jpeg"), b"one, edited").unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);

    let tree = remote.tree();
    assert_eq!(server_size(&remote, &tree["photos/one.jpeg"]), 11);
    assert!(a.folder.engine.trees.dirty().is_empty());
    assert_eq!(a.folder.view().problem, None);
}

fn swap_for_a_new_folder(folder: &Path) {
    let new = folder.with_extension("new");
    fs::create_dir(&new).unwrap();
    for entry in fs::read_dir(folder).unwrap() {
        let name = entry.unwrap().file_name();
        fs::rename(folder.join(&name), new.join(&name)).unwrap();
    }
    fs::remove_dir(folder).unwrap();
    fs::rename(&new, folder).unwrap();
}

#[test]
fn a_file_from_the_server_reaches_a_folder_swapped_for_another() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("photos")).unwrap();
    fs::write(site_a.root.join("photos/one.jpeg"), b"one").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);
    assert_eq!(fs::read(site_b.root.join("photos/one.jpeg")).unwrap(), b"one");

    swap_for_a_new_folder(&site_b.root.join("photos"));
    fs::write(site_a.root.join("photos/two.jpeg"), b"two").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(fs::read(site_b.root.join("photos/two.jpeg")).unwrap(), b"two");
}

