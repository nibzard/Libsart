//! Buffer tests for the Changes view and the plan controls (SPEC 5.2,
//! 5.4, 11.3-11.4, task T042).
//!
//! Every case drives the real shell against a real Git repository through
//! the shared operations, then renders into a ratatui test buffer. The
//! cases prove what the acceptance states: the column renders every
//! group's source context across the detail pages the diff operation
//! issues; the labels name kept, restored, undecided, unavailable, and
//! stale states in words; `K` and `r` change one in-memory plan only; the
//! source fallback and the unvisited scope counts stay available. The
//! independent oracle proves the read-only claim.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{Shell, TuiOptions, View, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::oracle;
use support::repo::TestRepo;

/// Runs the built binary in `cwd` with the isolated environment.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Runs one JSON query and returns its data object.
fn json_data(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> serde_json::Value {
    let output = margin(env, cwd, args);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the query must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON envelope");
    envelope
        .get("data")
        .cloned()
        .expect("a successful query carries data")
}

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// One key-press event.
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Renders one frame and returns its rows.
fn draw(shell: &mut Shell<'_>, columns: u16, rows: u16) -> Vec<String> {
    let mut terminal = TestTerminal::new(columns, rows);
    terminal.render(shell);
    terminal.page()
}

/// One test terminal: a ratatui terminal over a test buffer.
struct TestTerminal {
    terminal: Terminal<TestBackend>,
}

impl TestTerminal {
    fn new(columns: u16, rows: u16) -> TestTerminal {
        TestTerminal {
            terminal: Terminal::new(TestBackend::new(columns, rows))
                .expect("a test backend always initializes"),
        }
    }

    fn render(&mut self, shell: &mut Shell<'_>) {
        self.terminal
            .draw(|frame| render::draw(shell, frame))
            .expect("a test backend never fails to draw");
    }

    fn page(&self) -> Vec<String> {
        let buffer = self.terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

/// Joins rows for substring assertions.
fn joined(page: &[String]) -> String {
    page.join("\n")
}

/// One adopted repository: a start marker over `notes/` and one draft
/// checkpoint that edits one passage of `notes/a.md`.
fn adopted_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "init must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "checkpoint",
            "--message",
            "Edit the first passage",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "checkpoint must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo
}

/// One adopted repository whose pending review holds three groups in two
/// documents: `notes/a.md` edits two far-apart passages, `notes/b.md`
/// edits one.
fn three_group_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage.\n\nFiller one.\n\nFiller two.\n\nFiller three.\n\n\
           Filler four.\n\nSecond passage.\n",
    );
    repo.write_file("notes/b.md", b"# Two\n\nOther passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage, edited.\n\nFiller one.\n\nFiller two.\n\n\
           Filler three.\n\nFiller four.\n\nSecond passage, edited.\n",
    );
    repo.write_file("notes/b.md", b"# Two\n\nOther passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit three passages", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "checkpoint must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo
}

/// One adopted repository whose pending review holds more groups than one
/// detail page carries: fifty documents with one passage edit each, plus
/// one document with three far-apart passage edits.
fn paged_repo() -> TestRepo {
    let repo = TestRepo::init();
    for number in 1..=50 {
        let name = format!("notes/p{number:02}.md");
        repo.write_file(
            &name,
            format!("# P{number}\n\nOriginal passage {number}.\n").as_bytes(),
        );
    }
    repo.write_file(
        "notes/many.md",
        b"# Many\n\nFirst original.\n\nFiller one.\n\nFiller two.\n\nFiller three.\n\n\
           Filler four.\n\nSecond original.\n\nFiller five.\n\nFiller six.\n\n\
           Filler seven.\n\nFiller eight.\n\nThird original.\n",
    );
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    for number in 1..=50 {
        let name = format!("notes/p{number:02}.md");
        repo.write_file(
            &name,
            format!("# P{number}\n\nEdited passage {number}.\n").as_bytes(),
        );
    }
    repo.write_file(
        "notes/many.md",
        b"# Many\n\nFirst edited.\n\nFiller one.\n\nFiller two.\n\nFiller three.\n\n\
           Filler four.\n\nSecond edited.\n\nFiller five.\n\nFiller six.\n\n\
           Filler seven.\n\nFiller eight.\n\nThird edited.\n",
    );
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit every document", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "checkpoint must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo
}

#[test]
fn every_group_renders_its_context_across_the_detail_pages() {
    let repo = paged_repo();
    // The command line reads the same comparison through the same
    // operation; its whole-scope totals and pinned identity are the oracle
    // for what the surface must show (SPEC 6.3, 7.2).
    let data = json_data(repo.env(), repo.path(), &["diff", "--json"]);
    let totals = &data["scope_totals"];
    let groups = totals["groups"].as_u64().expect("group count") as usize;
    assert!(
        groups > 50,
        "the fixture must cross the default detail page of 50 groups, got {groups}",
    );
    assert_eq!(totals["files"].as_u64().expect("file count"), 51);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(
        repo.path(),
        &operations,
        &TuiOptions {
            view: View::Changes,
            ..TuiOptions::default()
        },
    );
    let changes = shell.changes().expect("the comparison loads");
    assert_eq!(changes.totals().groups, groups, "the whole scope");
    assert_eq!(changes.groups().len(), groups, "every group carried");
    assert!(
        changes.groups().len() > 50,
        "the view followed the detail pages past the first page",
    );
    assert_eq!(
        changes.branch_ref(),
        data["branch_ref"].as_str().expect("the branch ref"),
        "the surface pins the same branch",
    );
    assert_eq!(
        changes.candidate_oid(),
        data["candidate_oid"].as_str().expect("the candidate"),
        "the surface pins the same candidate C",
    );
    assert_eq!(
        changes.scope_id(),
        data["scope_id"].as_str().expect("the scope"),
        "the surface pins the same scope",
    );

    // Walk every group in manifest order and read what the column shows.
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains(&format!("{groups} passage groups")));
    assert!(screen.contains("Changed documents:"));
    assert!(screen.contains("notes/many.md · 3 groups"));
    let mut seen = 0;
    for index in 0..groups {
        let page = draw(&mut shell, 110, 30);
        let screen = joined(&page);
        assert!(
            screen.contains(&format!("› Group {} of {groups}", index + 1)),
            "group {index} shows its header with its state",
        );
        assert!(
            screen.contains("· Undecided"),
            "group {index} starts undecided",
        );
        // Every group shows the exact passage text of its after side; the
        // manifest orders files by path bytes, so the three spaced
        // passages of notes/many.md come first, then the fifty
        // single-passage documents.
        let expected = if index < 3 {
            ["First edited.", "Second edited.", "Third edited."][index].to_string()
        } else {
            format!("Edited passage {}.", index - 2)
        };
        assert!(
            screen.contains(&expected),
            "group {index} shows its changed passage {expected:?}",
        );
        seen += 1;
        shell.handle_event(&key(KeyCode::Char('n')));
    }
    assert_eq!(seen, groups, "the walk reached every group");
    // The last group holds; `n` states the end instead of wrapping.
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains(&format!("› Group {groups} of {groups}")));
    assert!(screen.contains("this is the last passage group"));
}

#[test]
fn k_and_r_change_only_the_in_memory_plan() {
    let repo = three_group_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    shell.handle_event(&key(KeyCode::Char('2')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("› Group 1 of 3 · Undecided"));
    assert!(
        screen.contains("r Restore previous passage"),
        "the passage action carries the SPEC 5.2 wording",
    );

    // `K` records one proposed keep; the label and the plan counts say so.
    shell.handle_event(&key(KeyCode::Char('K')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("› Group 1 of 3 · Kept"));
    assert!(screen.contains("proposed: keep this passage"), "the notice");
    assert!(
        screen.contains("1 kept · 0 restored · 2 undecided"),
        "the counts"
    );

    // `r` on the next group records a proposed restoration.
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('r')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("› Group 2 of 3 · Restored"));
    assert!(screen.contains("proposed: restore the previous passage"));

    // `r` again undoes the restoration; the passage is undecided again.
    shell.handle_event(&key(KeyCode::Char('r')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("› Group 2 of 3 · Undecided"));
    assert!(screen.contains("restoration undone"));
    // One navigation key clears the notice, so the context row states the
    // plan again.
    shell.handle_event(&key(KeyCode::Char('j')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("1 kept · 0 restored · 2 undecided"));

    // Lowercase `k` stays navigation (SPEC 11.3): it scrolls, it never
    // decides.
    shell.handle_event(&key(KeyCode::Char('k')));
    shell.handle_event(&key(KeyCode::Char('j')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("1 kept · 0 restored"),
        "navigation changed no decision",
    );

    // Only the in-memory plan moved: no repository byte changed, and the
    // working document still holds the edited passage.
    before.assert_unchanged(&oracle::snapshot(&repo), "the plan controls");
    assert!(
        repo.read_file("notes/a.md")
            .starts_with(b"# One\n\nFirst passage, edited.")
    );
}

#[test]
fn the_source_view_shows_the_exact_markdown_changes() {
    let repo = three_group_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));

    // The passage view reads as prose with both sides placed by line.
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("Before · line 3:"));
    assert!(screen.contains("First passage."));
    assert!(screen.contains("After · line 3:"));
    assert!(screen.contains("First passage, edited."));

    // `s` switches to the exact source: the canonical `@@` coordinates the
    // manifest states, with the exact bytes of both sides (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char('s')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("@@ -1,6 +1,6 @@"));
    assert!(screen.contains("-First passage."));
    assert!(screen.contains("+First passage, edited."));
    assert!(screen.contains("the exact Markdown changes"), "the notice");

    // `s` again returns the passage view; the source stays one key away.
    shell.handle_event(&key(KeyCode::Char('s')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(!screen.contains("@@ -1,6 +1,6 @@"));
    assert!(screen.contains("After · line 3:"));

    before.assert_unchanged(&oracle::snapshot(&repo), "the source toggle");
}

#[test]
fn opaque_changes_keep_an_exact_source_fallback() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.write_file("notes/data.md", b"# Data\n\nReadable text.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited.\n");
    repo.write_file(
        "notes/data.md",
        &[0xff, 0xfe, 0x00, 0x01, 0x02, b'b', b'i', b'n'],
    );
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "checkpoint",
            "--message",
            "Edit and capture opaque bytes",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('n')));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    // The opaque group is one whole-file decision, labeled in words with
    // its exact leading bytes and true length (SPEC 10.4, 11.4).
    assert!(screen.contains("notes/data.md"));
    assert!(screen.contains("opaque bytes"));
    assert!(screen.contains("whole file"));
    assert!(screen.contains("not valid UTF-8"));
    assert!(screen.contains("fffe00010262696e (8 bytes"));

    // The source view states why no line ranges apply and still shows the
    // exact readable side beside the opaque one.
    shell.handle_event(&key(KeyCode::Char('s')));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(screen.contains("no line ranges apply; the exact bytes of both sides are the source"),);
    assert!(screen.contains("-# Data"), "the readable before side");
    // The opaque group still carries a decision like any other.
    shell.handle_event(&key(KeyCode::Char('K')));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(screen.contains("· Kept"), "an opaque passage is decidable");
}

#[test]
fn stale_arrivals_relabel_every_group_and_refuse_decisions() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("· Undecided"),
        "the pinned comparison is current"
    );

    // New edits arrive: the agent's hook captures another draft, so the
    // head moves past the pinned candidate (SPEC 5.3).
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited again.\n");
    repo.write_file("notes/b.md", b"# Two\n\nSecond passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Later edits", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let after_arrival = oracle::snapshot(&repo);

    shell.handle_event(&key(KeyCode::Char('n')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("Stale"),
        "every group relabels itself stale"
    );
    assert!(
        screen.contains("New edits arrived. Refresh before applying."),
        "the SPEC 5.3 sentence shows",
    );

    // The keys refuse new decisions; the plan stays as it was.
    shell.handle_event(&key(KeyCode::Char('K')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("New edits arrived. Refresh before applying."),
        "the refusal names the stale arrival",
    );
    assert!(
        screen.contains("0 kept · 0 restored"),
        "no decision was recorded on the stale pair",
    );

    // Reading continues, and nothing the surface did wrote anything.
    shell.handle_event(&key(KeyCode::Char('p')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("First passage, edited."),
        "the text reads on"
    );
    after_arrival.assert_unchanged(&oracle::snapshot(&repo), "the stale surface");
}

#[test]
fn unavailable_decisions_when_no_comparison_can_be_built() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    let import = repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));

    // History rewritten away from the markers: no baseline exists, so the
    // comparison itself cannot be built (BASELINE_UNAVAILABLE).
    repo.git_ok(["reset", "--quiet", "--hard", &import]);
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("unavailable"),
        "the comparison states itself"
    );

    // The deciding keys refuse with a notice, never a crash or a write.
    shell.handle_event(&key(KeyCode::Char('K')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("no comparison exists to decide"));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('s')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("no comparison exists"), "every key answers");
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(shell.should_quit(), "an empty plan quits at once");
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused surface");
}

#[test]
fn unvisited_scope_counts_stay_available() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    // Entering the view visits the first group; the rest of the scope
    // stays counted, document by document (SPEC 5.2).
    shell.handle_event(&key(KeyCode::Char('2')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("3 passage groups"));
    assert!(screen.contains("2 changed documents"));
    assert!(screen.contains("0 kept · 0 restored · 3 undecided · 2 unvisited"));
    assert!(screen.contains("1 document holds no visited passage yet"));

    // A decision changes the counts the header states.
    shell.handle_event(&key(KeyCode::Char('K')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("Plan in memory: 1 kept · 0 restored · 2 undecided · 2 unvisited"),
        "the counts follow the decisions",
    );

    // Moving between groups scrolls the column, so the context row carries
    // the counts; the first document is fully visited, the second is not.
    shell.handle_event(&key(KeyCode::Char('n')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("1 kept · 0 restored · 2 undecided · 1 unvisited"));

    // The last group covers the second document; nothing is unvisited, and
    // scrolling back to the summary states the same.
    shell.handle_event(&key(KeyCode::Char('n')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("1 kept · 0 restored · 2 undecided · 0 unvisited"));
    shell.handle_event(&key(KeyCode::PageUp));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("0 unvisited"),
        "the summary states the covered scope",
    );
    assert!(
        !screen.contains("hold no visited passage yet"),
        "no unvisited document remains",
    );

    // The compact width keeps the unvisited count stated (SPEC 11.2).
    let page = draw(&mut shell, 70, 24);
    let screen = joined(&page);
    assert!(
        screen.contains("0 unvisited"),
        "the reduced row keeps the count"
    );
}

#[test]
fn quit_warns_once_about_unapplied_selections() {
    let repo = three_group_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    // Without decisions, quit needs no warning (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(shell.should_quit(), "an empty plan quits at once");

    // With one proposed selection, the first quit warns and keeps the
    // surface; the second discards the selections and quits (SPEC 5.2).
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(!shell.should_quit(), "the warning keeps the surface");
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("1 unapplied selection"),
        "the notice counts the unapplied selections",
    );
    assert!(screen.contains("Press q again to discard them and quit"));

    // Any other key clears the warning, so quitting still needs the
    // explicit second press.
    shell.handle_event(&key(KeyCode::Char('j')));
    let page = draw(&mut shell, 110, 30);
    assert!(!joined(&page).contains("Press q again"));
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(!shell.should_quit(), "the warning stands again after a key");
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(
        shell.should_quit(),
        "the explicit second quit discards them"
    );

    before.assert_unchanged(&oracle::snapshot(&repo), "the quit sequence");
}
