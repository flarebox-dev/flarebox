// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_first_link_holds_a_file_whose_name_the_space_holds_until_the_person_answers() {
    for answer in [Answer::Publish, Answer::Discard] {
        let seed = [act(1, 0, 0, 0, 0)];
        let local = [act(1, 0, 0, 0, 0), act(0, 0, 0, 2, 0)];
        let ran = run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &[],
            Cut::None,
            &[],
            answer,
            true,
        )
        .unwrap();
        assert_eq!(ran.asked, 1, "{answer}");
        let live = (ran.space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.node_id != ran.space.space_id())
            .count();
        let expected = if answer == Answer::Publish { 3 } else { 1 };
        assert_eq!(live, expected, "{answer}");
        assert_eq!(
            ran.removals,
            if answer == Answer::Discard { 2 } else { 0 },
            "{answer}"
        );
    }
}

#[test]
fn a_discarded_folder_the_user_moved_a_synced_file_into_before_the_answer_converges() {
    let seed = [
        act(1, 0, 0, 2, 0),
        act(0, 0, 0, 0, 0),
        act(0, 0, 0, 2, 0),
        act(1, 0, 5494772186727268893, 0, 0),
    ];
    let local = [act(0, 0, 0, 1, 0)];
    let during = [(
        true,
        vec![act(3, 3247345582551114674, 732025268020294025, 0, 0)],
    )];
    run_ignoring(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &[],
        Answer::Discard,
        true,
    )
    .unwrap();
}
