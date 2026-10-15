//! Workflow evidence for correcting part of a passage during review.
mod support;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::{
    app::{CheckpointRequest, InitRequest, Operations},
    domain::ScopeInput,
    git::Git,
    tui::{Shell, TuiOptions},
};
use support::repo::TestRepo;

fn press(shell: &mut Shell<'_>, code: KeyCode) {
    shell.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn checkpoint(ops: &Operations<'_>) -> bool {
    ops.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: Some("Correction fixture".into()),
        include_staged: false,
        expect_head: None,
        expect_index: None,
        expect_working: None,
        dry_run: false,
    })
    .ok()
}

fn adopted(before: &str, after: &str) -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", before.as_bytes());
    repo.write_file("outside.txt", b"Original outside content\n");
    repo.commit_all("Import");
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
    let ops = Operations::new(&git);
    assert!(
        ops.initialize_scope(&InitRequest {
            scope: Some(ScopeInput {
                include: vec!["content/".into()],
                suffixes: vec![".md".into()],
                ..Default::default()
            }),
            ..Default::default()
        })
        .ok()
    );
    repo.write_file("content/article.md", after.as_bytes());
    assert!(checkpoint(&ops));
    repo
}

fn document(time: &str, wording: &str, other: &str, count: usize) -> String {
    let mut text =
        format!("# Session\n\nThe session starts at {time}. The introduction is {wording}.\n");
    for index in 0..count {
        for gap in 0..5 {
            text.push_str(&format!("\nUnchanged context {index}-{gap}.\n"));
        }
        text.push_str(&format!("\nIndependent passage {index}: {other}.\n"));
    }
    text
}

fn correction_workflow(prior_decisions: usize, finish_first: bool) {
    let original = document(
        "10:00",
        "rather long and wordy",
        "original",
        prior_decisions,
    );
    let candidate = document("11:00", "concise", "revised", prior_decisions);
    let corrected = document(
        "10:00",
        "concise",
        if finish_first { "original" } else { "revised" },
        prior_decisions,
    );
    let intended = document("10:00", "concise", "original", prior_decisions);
    let repo = adopted(&original, &candidate);
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('2'));
    assert_eq!(shell.changes().unwrap().groups().len(), prior_decisions + 1);
    for _ in 0..prior_decisions {
        press(&mut shell, KeyCode::Char('n'));
        press(&mut shell, KeyCode::Char('r'));
    }
    for _ in 0..prior_decisions {
        press(&mut shell, KeyCode::Char('p'));
    }
    assert_eq!(shell.changes().unwrap().proposed_count(), prior_decisions);
    let old_restore_ids = shell.changes().unwrap().restore_selection();
    if finish_first {
        press(&mut shell, KeyCode::Enter);
        press(&mut shell, KeyCode::Enter);
        assert!(
            shell.finish_result().unwrap().ok(),
            "{:?}",
            shell.finish_result().unwrap().error
        );
        assert_eq!(
            std::fs::read_to_string(repo.file_path("content/article.md")).unwrap(),
            document("11:00", "concise", "original", prior_decisions)
        );
    }
    let before_save = repo.git_ok(["rev-parse", "HEAD"]);
    press(&mut shell, KeyCode::Char('e'));
    assert!(
        shell.source_editor().is_some(),
        "editing opens directly from Changes"
    );
    // Replace the buffer to simulate an exact correction. This does not measure
    // human typing, selection, discovery time, or physical terminal behavior.
    shell.handle_event(&Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL,
    )));
    shell.handle_event(&Event::Paste(corrected.clone()));
    press(&mut shell, KeyCode::Esc);
    assert!(shell.source_editor().is_none());
    assert_eq!(
        std::fs::read_to_string(repo.file_path("content/article.md")).unwrap(),
        corrected
    );
    assert_eq!(repo.git_ok(["rev-parse", "HEAD"]), before_save);
    press(&mut shell, KeyCode::Char('C'));
    let retained = if finish_first { 0 } else { prior_decisions };
    if retained > 0 {
        assert!(
            shell
                .notice()
                .unwrap()
                .contains(&format!("{retained} review decisions retained; 0 reset"))
        );
        let new_ids = shell.changes().unwrap().restore_selection();
        assert_eq!(new_ids.len(), retained);
        assert!(
            new_ids.iter().all(|id| !old_restore_ids.contains(id)),
            "retained decisions use new review identities"
        );
    }
    assert_ne!(repo.git_ok(["rev-parse", "HEAD"]), before_save);
    assert_eq!(shell.changes().unwrap().proposed_count(), retained);
    press(&mut shell, KeyCode::Enter);
    press(&mut shell, KeyCode::Enter);
    assert!(shell.finish_result().unwrap().ok());
    assert_eq!(
        std::fs::read_to_string(repo.file_path("content/article.md")).unwrap(),
        intended
    );
    println!(
        "prior_decisions={prior_decisions}; finish_first={finish_first}; decisions_reentered=0; exact_result=true"
    );
}

#[test]
fn correct_a_mixed_passage_before_other_decisions() {
    correction_workflow(0, false);
}

#[test]
fn correct_a_mixed_passage_after_one_restore_decision() {
    correction_workflow(1, false);
}

#[test]
fn correct_a_mixed_passage_after_five_restore_decisions() {
    correction_workflow(5, false);
}

#[test]
fn finishing_first_avoids_reentry_but_approves_the_unwanted_time() {
    correction_workflow(5, true);
}

fn transfer_after_external_correction(
    before: &str,
    candidate: &str,
    corrected: &str,
    retained: usize,
    expected: &str,
) {
    let repo = adopted(before, candidate);
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('2'));
    let count = shell.changes().unwrap().groups().len();
    for _ in 0..count {
        press(&mut shell, KeyCode::Char('r'));
        press(&mut shell, KeyCode::Char('n'));
    }
    assert_eq!(shell.changes().unwrap().proposed_count(), count);
    repo.write_file("content/article.md", corrected.as_bytes());
    repo.write_file("outside.txt", b"Uncaptured outside edit\n");
    press(&mut shell, KeyCode::Char('C'));
    assert_eq!(shell.changes().unwrap().proposed_count(), retained);
    assert!(shell.notice().unwrap().contains(&format!(
        "{retained} review decisions retained; {} reset",
        count - retained,
    )));
    if !shell.changes().unwrap().groups().is_empty() {
        press(&mut shell, KeyCode::Enter);
        press(&mut shell, KeyCode::Enter);
        assert!(
            shell.finish_result().unwrap().ok(),
            "{:?}",
            shell.finish_result().unwrap().error
        );
    }
    assert_eq!(
        std::fs::read_to_string(repo.file_path("content/article.md")).unwrap(),
        expected
    );
    assert_eq!(
        repo.git_ok(["show", "HEAD:outside.txt"]),
        b"Original outside content\n"
    );
    assert_eq!(
        std::fs::read(repo.file_path("outside.txt")).unwrap(),
        b"Uncaptured outside edit\n"
    );
}

#[test]
fn changed_decisions_reset_and_unchanged_decisions_survive_external_corrections() {
    let before = document("10:00", "wordy", "original", 1);
    let candidate = document("11:00", "concise", "revised", 1);
    let corrected = document("10:00", "concise", "revised", 1);
    let expected = document("10:00", "concise", "original", 1);
    transfer_after_external_correction(&before, &candidate, &corrected, 1, &expected);
}

#[test]
fn a_removed_group_resets_its_decision_without_resetting_another_group() {
    let before = document("10:00", "wordy", "original", 1);
    let candidate = document("11:00", "concise", "revised", 1);
    let corrected = document("10:00", "wordy", "revised", 1);
    transfer_after_external_correction(&before, &candidate, &corrected, 1, &before);
}

#[test]
fn shifted_patch_coordinates_reset_decisions_conservatively() {
    let before = document("10:00", "wordy", "original", 1);
    let candidate = document("11:00", "concise", "revised", 1);
    let corrected = format!("New preface.\n\n{candidate}");
    transfer_after_external_correction(&before, &candidate, &corrected, 0, &corrected);
}

#[test]
fn hidden_changes_in_a_long_passage_reset_its_decision() {
    let padding = "Unchanged sentence. ".repeat(3000);
    let before = format!("# Long\n\n{padding}Original ending.\n");
    let candidate = format!("# Long\n\n{padding}First replacement.\n");
    let corrected = format!("# Long\n\n{padding}Second replacement.\n");
    transfer_after_external_correction(&before, &candidate, &corrected, 0, &corrected);
}

#[test]
fn an_unchanged_checkpoint_retains_decisions() {
    let before = document("10:00", "wordy", "original", 1);
    let candidate = document("11:00", "concise", "revised", 1);
    transfer_after_external_correction(&before, &candidate, &candidate, 2, &before);
}

#[test]
fn a_failed_checkpoint_keeps_the_existing_plan() {
    let before = document("10:00", "wordy", "original", 1);
    let candidate = document("11:00", "concise", "revised", 1);
    let repo = adopted(&before, &candidate);
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('2'));
    press(&mut shell, KeyCode::Char('r'));
    let decisions = shell.changes().unwrap().restore_selection();
    repo.write_file("outside.txt", b"Unrelated staging\n");
    repo.git_ok(["add", "outside.txt"]);
    let index = repo.git_ok(["write-tree"]);
    let head = repo.git_ok(["rev-parse", "HEAD"]);
    press(&mut shell, KeyCode::Char('C'));
    assert_eq!(shell.changes().unwrap().restore_selection(), decisions);
    assert_eq!(repo.git_ok(["write-tree"]), index);
    assert_eq!(repo.git_ok(["rev-parse", "HEAD"]), head);
    assert!(shell.notice().unwrap().contains("STAGED_CHANGES"));
}

#[test]
fn keep_and_restore_decisions_survive_with_new_ids() {
    let before = document("10:00", "wordy", "original", 2);
    let candidate = document("11:00", "concise", "revised", 2);
    let corrected = document("10:00", "concise", "revised", 2);
    let repo = adopted(&before, &candidate);
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('2'));
    press(&mut shell, KeyCode::Char('K')); // Changed group must reset.
    press(&mut shell, KeyCode::Char('n'));
    press(&mut shell, KeyCode::Char('K')); // Unchanged keep.
    press(&mut shell, KeyCode::Char('n'));
    press(&mut shell, KeyCode::Char('r')); // Unchanged restore.
    let old_restore = shell.changes().unwrap().restore_selection();
    repo.write_file("content/article.md", corrected.as_bytes());
    press(&mut shell, KeyCode::Char('C'));
    let counts = shell.changes().unwrap().counts();
    assert_eq!((counts.kept, counts.restored, counts.undecided), (1, 1, 1));
    assert!(
        shell
            .notice()
            .unwrap()
            .contains("2 review decisions retained; 1 reset")
    );
    assert_ne!(shell.changes().unwrap().restore_selection(), old_restore);
    press(&mut shell, KeyCode::Enter);
    press(&mut shell, KeyCode::Enter);
    assert!(shell.finish_result().unwrap().ok());
    let expected = corrected.replace(
        "Independent passage 1: revised.",
        "Independent passage 1: original.",
    );
    assert_eq!(
        std::fs::read_to_string(repo.file_path("content/article.md")).unwrap(),
        expected
    );
}

#[test]
fn repeated_passages_do_not_transfer_a_changed_occurrences_decision() {
    let filler = "\n\nContext one.\n\nContext two.\n\nContext three.\n\nContext four.\n\n";
    let before = format!("# Repeated\n\nSame original passage.{filler}Same original passage.\n");
    let candidate = before.replace("Same original passage.", "Same revised passage.");
    let corrected = candidate.replacen("Same revised passage.", "Same corrected passage.", 1);
    let expected = corrected.replace("Same revised passage.", "Same original passage.");
    transfer_after_external_correction(&before, &candidate, &corrected, 1, &expected);
}

#[test]
fn decisions_do_not_cross_a_new_start_or_scope() {
    for change_scope in [false, true] {
        let before = document("10:00", "wordy", "original", 1);
        let candidate = document("11:00", "concise", "revised", 1);
        let repo = adopted(&before, &candidate);
        let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
        let ops = Operations::new(&git);
        let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut shell, KeyCode::Char('2'));
        press(&mut shell, KeyCode::Char('r'));
        let scope = ScopeInput {
            include: vec![
                if change_scope {
                    "content/article.md"
                } else {
                    "content/"
                }
                .into(),
            ],
            suffixes: vec![".md".into()],
            ..Default::default()
        };
        assert!(
            ops.initialize_scope(&InitRequest {
                scope: Some(scope),
                new_start: true,
                ..Default::default()
            })
            .ok()
        );
        repo.write_file(
            "content/article.md",
            candidate.replace("concise", "short").as_bytes(),
        );
        press(&mut shell, KeyCode::Char('C'));
        assert_eq!(shell.changes().unwrap().proposed_count(), 0);
        assert!(
            shell
                .notice()
                .unwrap()
                .contains("0 review decisions retained; 1 reset")
        );
    }
}
