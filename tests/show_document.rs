//! Real-Git tests for one historical document view (SPEC 6.2-6.3, task T021).
//!
//! Every case proves one contract of `show` against the installed Git:
//!
//! - the query distinguishes a missing document, an empty document,
//!   readable UTF-8, and opaque bytes at one immutable revision, and the
//!   reported blob and revision match what Git itself resolves
//! - the revision is immutable: later commits and working edits never
//!   change what an earlier revision answers
//! - JSON and text output obey the byte and line bounds with truncation
//!   metadata, escape their content, and never carry a raw terminal
//!   control byte
//! - the explicit raw export returns every exact byte, stays exclusive
//!   with the envelope and the bounds, and refuses a terminal destination
//!   unless the caller explicitly allows one
//! - unusual names, both object formats, directories, symbolic links,
//!   and gitlinks behave exactly, and every query leaves the repository
//!   untouched

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{DocumentContent, Operations, ShowRequest};
use margin::git::Git;
use serde_json::Value;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};

/// Runs the installed executable once with the isolated environment.
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

/// The exit class of one finished binary run.
fn exit_of(output: &Output) -> i32 {
    output.status.code().unwrap_or_else(|| {
        panic!("the binary died from a signal");
    })
}

/// Parses the one JSON envelope of a successful machine-mode run and checks
/// the envelope contract itself (SPEC 7.1).
fn data_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches('\n').count(),
        1,
        "{what}: exactly one envelope line, got: {:?}",
        stdout.chars().take(400).collect::<String>()
    );
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}"));
    assert_eq!(envelope["ok"], true, "{what}: the query succeeds");
    assert_eq!(envelope["error"], Value::Null, "{what}: no error");
    assert_eq!(
        envelope["effects"]["working_tree"], "unchanged",
        "{what}: a query writes no file"
    );
    assert_eq!(
        envelope["effects"]["index"], "unchanged",
        "{what}: a query writes no index entry"
    );
    assert_eq!(
        envelope["effects"]["commit"], "none",
        "{what}: a query creates no commit"
    );
    assert!(
        output.stderr.is_empty(),
        "{what}: machine mode writes no stderr"
    );
    envelope["data"].clone()
}

/// The error code of one failed envelope.
fn code_of(envelope: &Value) -> &str {
    envelope["error"]["code"].as_str().unwrap()
}

/// One failed envelope, parsed.
fn error_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["ok"], false, "{what}: the query fails");
    envelope
}

/// The blob object ID Git itself records for one path at one revision.
fn blob_of(repo: &TestRepo, revision: &str, path: &str) -> String {
    support::text(&repo.git_ok(["rev-parse", &format!("{revision}:{path}")]))
}

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

#[test]
fn utf8_documents_report_exact_content_and_identity() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# Title\n\nBody line.\n");
    let revision = repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "a readable document");

    assert_eq!(data["revision"], revision, "the resolved immutable commit");
    assert_eq!(data["path"], "notes/a.md");
    assert_eq!(data["object_format"], "sha1");
    assert_eq!(data["content"]["kind"], "text");
    assert_eq!(data["content"]["mode"], "100644");
    assert_eq!(
        data["content"]["blob_oid"],
        blob_of(&repo, "HEAD", "notes/a.md")
    );
    assert_eq!(data["content"]["text"], "# Title\n\nBody line.\n");
    assert_eq!(data["content"]["total_bytes"], 20);
    assert_eq!(data["content"]["returned_bytes"], 20);
    assert_eq!(data["content"]["total_lines"], 3);
    assert_eq!(data["content"]["returned_lines"], 3);
    assert_eq!(data["content"]["truncated"], false);

    // The query leaves the repository untouched.
    let before = support::oracle::snapshot(&repo);
    margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the show query");
}

#[test]
fn missing_and_empty_documents_stay_distinct() {
    let repo = TestRepo::init();
    repo.write_file("notes/empty.md", b"");
    repo.write_file("notes/full.md", b"words\n");
    repo.commit_all("import");

    let empty = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/empty.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(
        exit_of(&empty),
        0,
        "an empty document is a successful query"
    );
    let data = data_of(&empty, "an empty document");
    assert_eq!(data["content"]["kind"], "empty");
    assert_eq!(data["content"]["total_bytes"], 0);
    assert_eq!(
        data["content"]["blob_oid"],
        blob_of(&repo, "HEAD", "notes/empty.md"),
        "Git's own empty blob object ID"
    );

    let missing = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/never-here.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&missing), 0, "absence is an answer, not a failure");
    let data = data_of(&missing, "an absent document");
    assert_eq!(data["content"]["kind"], "missing");
    assert!(
        data["content"].get("blob_oid").is_none(),
        "an absent document names no blob"
    );

    // A raw export keeps the two states distinguishable: zero bytes for an
    // empty document, one typed refusal for an absent path.
    let raw_empty = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/empty.md",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
    );
    assert_eq!(exit_of(&raw_empty), 0);
    assert!(
        raw_empty.stdout.is_empty(),
        "an empty document exports nothing"
    );

    let raw_missing = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/never-here.md",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
    );
    assert_eq!(
        exit_of(&raw_missing),
        2,
        "an absent path has no bytes to export"
    );
    assert!(raw_missing.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&raw_missing.stderr).contains("INVALID_ARGUMENT"),
        "got: {}",
        String::from_utf8_lossy(&raw_missing.stderr)
    );
}

#[test]
fn revisions_are_immutable_against_later_history_and_edits() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"first version\n");
    let first = repo.commit_all("first");
    repo.write_file("notes/a.md", b"second version\n");
    let second = repo.commit_all("second");
    // Working bytes newer than every commit.
    repo.write_file("notes/a.md", b"unsaved third version\n");

    for (revision, expected) in [
        (first.as_str(), "first version\n"),
        (second.as_str(), "second version\n"),
    ] {
        let output = margin(
            repo.env(),
            repo.path(),
            &[
                "show",
                "--path",
                "notes/a.md",
                "--revision",
                revision,
                "--json",
            ],
        );
        assert_eq!(exit_of(&output), 0);
        let data = data_of(&output, "one immutable revision");
        assert_eq!(data["revision"], revision);
        assert_eq!(data["content"]["text"], expected);
        assert_eq!(
            data["content"]["blob_oid"],
            blob_of(&repo, revision, "notes/a.md")
        );
    }

    // A symbolic revision resolves once, then the same commit answers.
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD~1",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "a relative revision");
    assert_eq!(data["revision"], first);
    assert_eq!(data["content"]["text"], "first version\n");
}

#[test]
fn byte_and_line_bounds_carry_truncation_metadata() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"line1\nline2\nline3\n");
    repo.commit_all("import");

    let bounded = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--max-bytes",
            "7",
            "--json",
        ],
    );
    assert_eq!(exit_of(&bounded), 0);
    let data = data_of(&bounded, "a byte-bounded view");
    assert_eq!(data["content"]["text"], "line1\nl");
    assert_eq!(data["content"]["total_bytes"], 18);
    assert_eq!(data["content"]["returned_bytes"], 7);
    assert_eq!(data["content"]["total_lines"], 3);
    assert_eq!(data["content"]["returned_lines"], 2);
    assert_eq!(data["content"]["truncated"], true);

    let lines = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--max-lines",
            "2",
            "--json",
        ],
    );
    assert_eq!(exit_of(&lines), 0);
    let data = data_of(&lines, "a line-bounded view");
    assert_eq!(data["content"]["text"], "line1\nline2\n");
    assert_eq!(data["content"]["returned_bytes"], 12);
    assert_eq!(data["content"]["returned_lines"], 2);
    assert_eq!(data["content"]["truncated"], true);

    // The text view states the truncation too.
    let text = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--max-lines",
            "1",
        ],
    );
    assert_eq!(exit_of(&text), 0);
    let body = String::from_utf8_lossy(&text.stdout);
    assert!(body.contains("content=text"), "got: {body}");
    assert!(
        body.contains("truncated after 6 of 18 bytes"),
        "got: {body}"
    );
    assert!(body.contains("line1\n"), "got: {body}");
}

#[test]
fn a_document_beyond_the_per_file_ceiling_fails_typed() {
    let repo = TestRepo::init();
    // One byte past the 16 MiB per-file ceiling of SPEC 3.3: the read
    // itself refuses with one typed limit instead of truncating source
    // bytes.
    let oversized = vec![b'a'; 16 * 1024 * 1024 + 1];
    repo.write_file("notes/huge.md", &oversized);
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/huge.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 2, "the ceiling is one typed limit");
    let envelope = error_of(&output, "an oversized document");
    assert_eq!(code_of(&envelope), "LIMIT_EXCEEDED");
    assert!(output.stdout.len() < 4 * 1024, "the refusal stays bounded");
}

#[test]
fn byte_bounds_cut_at_character_boundaries() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", "héllo wörld\n".as_bytes());
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--max-bytes",
            "2",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "a multibyte cut");
    // Two bytes cover only `h`; the two-byte é stays whole or drops.
    assert_eq!(data["content"]["text"], "h");
    assert_eq!(data["content"]["returned_bytes"], 1);
    assert_eq!(data["content"]["truncated"], true);
}

#[test]
fn json_and_text_output_escape_unsafe_content() {
    let repo = TestRepo::init();
    let nasty = "quote\" backslash\\ esc\u{1b}[31m bell\u{7} tab\t end\n";
    repo.write_file("notes/a.md", nasty.as_bytes());
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    // The envelope itself stays one line and parses; the carried text is
    // exactly the document.
    let data = data_of(&output, "escaped JSON");
    assert_eq!(data["content"]["text"], nasty);
    assert!(
        !output.stdout.contains(&0x1b),
        "JSON output carries no raw escape byte"
    );
    let body = String::from_utf8_lossy(&output.stdout);
    assert!(
        body.contains("\\u001b"),
        "the control byte is JSON-escaped, got: {body}"
    );

    let text = margin(
        repo.env(),
        repo.path(),
        &["show", "--path", "notes/a.md", "--revision", "HEAD"],
    );
    assert_eq!(exit_of(&text), 0);
    let body = String::from_utf8_lossy(&text.stdout);
    assert!(
        !body.contains('\u{1b}'),
        "the text view carries no raw escape byte, got: {body:?}"
    );
    assert!(
        body.contains("<U+001B>[31m") && body.contains("<U+0007>"),
        "unsafe controls become visible escapes, got: {body:?}"
    );
    assert!(
        body.contains("tab\t") && body.contains(" end\n"),
        "the newline and tab a view needs survive, got: {body:?}"
    );
}

#[test]
fn opaque_documents_use_one_bounded_hexadecimal_view() {
    let repo = TestRepo::init();
    let bytes: Vec<u8> = vec![0xff, 0xfe, 0x00, 0x41, 0x42];
    repo.write_file("notes/data.bin", &bytes);
    repo.commit_all("import");
    let oid = blob_of(&repo, "HEAD", "notes/data.bin");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/data.bin",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "an opaque document");
    assert_eq!(data["content"]["kind"], "opaque");
    assert_eq!(data["content"]["blob_oid"], oid);
    assert_eq!(data["content"]["total_bytes"], 5);
    assert_eq!(data["content"]["returned_bytes"], 5);
    assert_eq!(data["content"]["hex"], "fffe004142");
    assert_eq!(data["content"]["truncated"], false);

    // One bound smaller than the document truncates the hexadecimal view.
    let bounded = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/data.bin",
            "--revision",
            "HEAD",
            "--max-bytes",
            "2",
            "--json",
        ],
    );
    assert_eq!(exit_of(&bounded), 0);
    let data = data_of(&bounded, "a bounded opaque view");
    assert_eq!(data["content"]["hex"], "fffe");
    assert_eq!(data["content"]["returned_bytes"], 2);
    assert_eq!(data["content"]["total_bytes"], 5);
    assert_eq!(data["content"]["truncated"], true);
}

#[test]
fn the_hexadecimal_view_caps_at_its_own_bound() {
    let repo = TestRepo::init();
    // A little more than the 64 KiB hexadecimal budget of SPEC 6.3.
    let bytes = vec![0xff_u8; margin::domain::MAX_SHOW_HEX_BYTES + 10];
    repo.write_file("notes/big.bin", &bytes);
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/big.bin",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "an oversized opaque document");
    assert_eq!(data["content"]["kind"], "opaque");
    assert_eq!(
        data["content"]["total_bytes"],
        (margin::domain::MAX_SHOW_HEX_BYTES + 10) as u64
    );
    assert_eq!(
        data["content"]["returned_bytes"],
        margin::domain::MAX_SHOW_HEX_BYTES as u64
    );
    let hex = data["content"]["hex"].as_str().unwrap();
    assert_eq!(
        hex.len(),
        margin::domain::MAX_SHOW_HEX_BYTES * 2,
        "the hexadecimal view carries exactly its budget"
    );
    assert_eq!(data["content"]["truncated"], true);
}

#[test]
fn the_raw_export_returns_every_exact_byte() {
    let repo = TestRepo::init();
    // Deliberately invalid UTF-8 with no trailing newline.
    let bytes: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0xff, 0x0d, 0x0a, 0x1a, 0x0a, 0x00];
    repo.write_file("notes/pic.png", &bytes);
    repo.commit_all("import");
    let oid = blob_of(&repo, "HEAD", "notes/pic.png");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/pic.png",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    assert!(
        output.stderr.is_empty(),
        "a raw export writes no stderr, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout, bytes,
        "the export equals the exact blob bytes"
    );
    assert_eq!(
        output.stdout,
        repo.git_ok(["cat-file", "blob", &oid]),
        "an independent Git read agrees"
    );

    // The terminal protection never blocks redirection, and one explicit
    // allow changes nothing for a piped destination.
    let allowed = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/pic.png",
            "--revision",
            "HEAD",
            "--format",
            "raw",
            "--allow-tty",
        ],
    );
    assert_eq!(exit_of(&allowed), 0);
    assert_eq!(allowed.stdout, bytes);
}

#[test]
fn the_raw_export_stays_exclusive_with_the_envelope_and_bounds() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"content\n");
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "--json",
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
    );
    assert_eq!(exit_of(&output), 2, "raw and --json are mutually exclusive");
    // The refusal itself still answers in the one JSON envelope.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("the refusal envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["command"], "show");
    assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");

    for bound in ["--max-bytes", "--max-lines"] {
        let output = margin(
            repo.env(),
            repo.path(),
            &[
                "show",
                "--path",
                "notes/a.md",
                "--revision",
                "HEAD",
                "--format",
                "raw",
                bound,
                "3",
            ],
        );
        assert_eq!(exit_of(&output), 2, "{bound} cannot bound exact bytes");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("INVALID_ARGUMENT"),
            "got: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn unresolvable_revisions_and_shapes_are_invalid_input() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("notes/deeper/b.md", b"# Two\n");
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "no-such-revision",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 2);
    let envelope = error_of(&output, "an unresolvable revision");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");

    // A leading dash stays data through the end-of-options boundary.
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "-nonsense",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 2);
    assert_eq!(
        code_of(&error_of(&output, "a dashed revision")),
        "INVALID_ARGUMENT"
    );

    // A directory names no one document.
    let output = margin(
        repo.env(),
        repo.path(),
        &["show", "--path", "notes", "--revision", "HEAD", "--json"],
    );
    assert_eq!(exit_of(&output), 2);
    assert_eq!(
        code_of(&error_of(&output, "a directory path")),
        "INVALID_ARGUMENT"
    );

    // A trailing separator is rejected before Git runs.
    let output = margin(
        repo.env(),
        repo.path(),
        &["show", "--path", "notes/", "--revision", "HEAD", "--json"],
    );
    assert_eq!(exit_of(&output), 2);
    assert_eq!(
        code_of(&error_of(&output, "a trailing separator")),
        "INVALID_ARGUMENT"
    );
}

#[test]
fn symbolic_links_and_gitlinks_are_not_documents() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let notes = repo.file_path("notes");
    std::os::unix::fs::symlink("a.md", notes.join("alias.md")).unwrap();
    repo.git_ok(["add", "--all"]);
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/alias.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 2);
    let envelope = error_of(&output, "a symbolic link");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("symbolic link"),
        "the message names the stored kind"
    );

    // One gitlink entry without a real submodule: staged through Git's own
    // cacheinfo protocol, then committed.
    let repo = TestRepo::init();
    let phantom = "1".repeat(40);
    repo.git_stdin(
        [
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{phantom},sub"),
        ],
        b"",
    );
    repo.git_ok(["commit", "--quiet", "--message", "gitlink"]);

    let output = margin(
        repo.env(),
        repo.path(),
        &["show", "--path", "sub", "--revision", "HEAD", "--json"],
    );
    assert_eq!(exit_of(&output), 2);
    let envelope = error_of(&output, "a gitlink");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("gitlink"),
        "the message names the stored kind"
    );
}

#[test]
fn unusual_paths_show_exactly() {
    let repo = TestRepo::init();
    let names = [
        "notes/my draft v2.md",
        "notes/quote\"d.md",
        "notes/ünicode-草稿.md",
        "notes/-leading-dash.md",
    ];
    for (index, name) in names.iter().enumerate() {
        repo.write_file(name, format!("content {index}\n").as_bytes());
    }
    repo.commit_all("import");

    for (index, name) in names.iter().enumerate() {
        let expected = format!("content {index}\n");
        let output = margin(
            repo.env(),
            repo.path(),
            &["show", "--path", name, "--revision", "HEAD", "--json"],
        );
        assert_eq!(exit_of(&output), 0, "{name}: the query succeeds");
        let data = data_of(&output, name);
        assert_eq!(data["path"], *name);
        assert_eq!(data["content"]["text"], expected);
        assert_eq!(data["content"]["blob_oid"], blob_of(&repo, "HEAD", name));

        let raw = margin(
            repo.env(),
            repo.path(),
            &[
                "show",
                "--path",
                name,
                "--revision",
                "HEAD",
                "--format",
                "raw",
            ],
        );
        assert_eq!(exit_of(&raw), 0, "{name}: the export succeeds");
        assert_eq!(raw.stdout, expected.as_bytes());
    }

    // Redundant separators normalize to the same document.
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "./notes//my draft v2.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "a normalized path");
    assert_eq!(data["path"], "notes/my draft v2.md");
    assert_eq!(data["content"]["text"], "content 0\n");
}

#[test]
fn executable_documents_report_their_mode() {
    let repo = TestRepo::init();
    let path = repo.write_file("notes/run.md", b"#!/usr/bin/env margin\n");
    repo.set_mode("notes/run.md", 0o755);
    repo.git_ok(["add", "--all"]);
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/run.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "an executable document");
    assert_eq!(data["content"]["mode"], "100755");
    assert!(path.is_file());
}

#[test]
fn the_sha256_object_format_reports_full_ids() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    repo.write_file("notes/a.md", b"sha256 body\n");
    let revision = repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "a sha256 document");
    assert_eq!(data["object_format"], "sha256");
    assert_eq!(data["revision"], revision);
    assert_eq!(data["revision"].as_str().unwrap().len(), 64);
    assert_eq!(data["content"]["kind"], "text");
    assert_eq!(data["content"]["text"], "sha256 body\n");

    let raw = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
    );
    assert_eq!(exit_of(&raw), 0);
    assert_eq!(raw.stdout, b"sha256 body\n");
}

#[test]
fn the_query_needs_no_adoption_and_refuses_bare_repositories() {
    // One ordinary repository that margin never initialized still reads.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"before adoption\n");
    repo.commit_all("import");
    let settings = repo.git(["config", "--local", "--get", "margin.scope"]);
    assert!(!settings.status.success(), "no margin settings exist yet");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0);
    let data = data_of(&output, "before adoption");
    assert_eq!(data["content"]["text"], "before adoption\n");

    // Outside any repository the query reports the typed state.
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = support::env::IsolatedEnv::new(home.path());
    let output = margin(
        &env,
        outside.path(),
        &["show", "--path", "a.md", "--revision", "HEAD", "--json"],
    );
    assert_eq!(exit_of(&output), 4);
    let envelope = error_of(&output, "outside any repository");
    assert_eq!(code_of(&envelope), "NOT_INITIALIZED");
}

#[test]
fn every_show_leaves_the_repository_unchanged() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"one\n");
    repo.write_file("notes/data.bin", &[0xff, 0x00, 0xfe]);
    repo.commit_all("import");
    repo.write_file("notes/a.md", b"one\nworking\n");
    let before = support::oracle::snapshot(&repo);

    for args in [
        vec![
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
        vec!["show", "--path", "notes/a.md", "--revision", "HEAD"],
        vec![
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--max-bytes",
            "2",
            "--json",
        ],
        vec![
            "show",
            "--path",
            "notes/data.bin",
            "--revision",
            "HEAD",
            "--json",
        ],
        vec![
            "show",
            "--path",
            "notes/data.bin",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
        vec![
            "show",
            "--path",
            "notes/absent.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 0, "{args:?}");
    }
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the show queries");
}

#[test]
fn a_real_terminal_destination_refuses_the_raw_export() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"terminal-protected bytes\n");
    repo.commit_all("import");
    let binary = env!("CARGO_BIN_EXE_margin");

    // `script` runs the command with a genuine pseudo-terminal on stdout,
    // which no standard-library test helper provides.
    let run_on_terminal = |command: &str| {
        let mut script = Command::new("script");
        script
            .args(["-qec", command, "/dev/null"])
            .current_dir(repo.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            if support::env::is_scrubbed(&key) {
                script.env_remove(&key);
            }
        }
        script.envs(repo.env().vars());
        match script.output() {
            Ok(output) => Some(output),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot run script: {error}"),
        }
    };

    let Some(refusal) = run_on_terminal(&format!(
        "{binary} show --path notes/a.md --revision HEAD --format raw"
    )) else {
        eprintln!("script is unavailable; the terminal decision stays covered by the unit test");
        return;
    };
    assert_eq!(exit_of(&refusal), 2, "a terminal destination refuses");
    let seen = String::from_utf8_lossy(&refusal.stdout);
    assert!(
        seen.contains("INVALID_ARGUMENT"),
        "the refusal is one typed line, got: {seen}"
    );
    assert!(
        !seen.contains("terminal-protected bytes"),
        "no document byte reaches the terminal, got: {seen}"
    );

    // A view escapes instead of refusing, and one explicit allow exports.
    let view = run_on_terminal(&format!("{binary} show --path notes/a.md --revision HEAD"))
        .expect("script ran once already");
    assert_eq!(exit_of(&view), 0, "a display-escaped view still works");
    assert!(String::from_utf8_lossy(&view.stdout).contains("terminal-protected bytes"));

    let allowed = run_on_terminal(&format!(
        "{binary} show --path notes/a.md --revision HEAD --format raw --allow-tty"
    ))
    .expect("script ran once already");
    assert_eq!(exit_of(&allowed), 0, "an explicit allow exports");
    assert!(String::from_utf8_lossy(&allowed.stdout).contains("terminal-protected bytes"));
}

#[test]
fn the_shared_operation_reports_the_same_content_states() {
    // The interface layer only serializes: the operation itself already
    // distinguishes the four content states without any interface.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"readable\n");
    repo.write_file("notes/empty.md", b"");
    repo.write_file("notes/data.bin", &[0xff]);
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let revision = support::text(&repo.git_ok(["rev-parse", "HEAD"]));

    let show = |path: &str| {
        operations.show_document(&ShowRequest {
            path: path.to_string(),
            revision: revision.clone(),
            max_bytes: None,
            max_lines: None,
            raw: false,
        })
    };

    let outcome = show("notes/a.md");
    assert!(outcome.ok());
    match &outcome.data.as_ref().unwrap().content {
        DocumentContent::Text(text) => {
            assert_eq!(text.text, "readable\n");
            assert!(!text.truncated);
        }
        other => panic!("a readable document is text, got {other:?}"),
    }

    match &show("notes/empty.md").data.unwrap().content {
        DocumentContent::Empty { .. } => {}
        other => panic!("a zero-byte document is empty, got {other:?}"),
    }

    match &show("notes/data.bin").data.unwrap().content {
        DocumentContent::Opaque(view) => assert_eq!(view.hex, "ff"),
        other => panic!("a non-UTF-8 document is opaque, got {other:?}"),
    }

    match &show("notes/absent.md").data.unwrap().content {
        DocumentContent::Missing => {}
        other => panic!("an absent document is missing, got {other:?}"),
    }
}
