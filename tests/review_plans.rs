//! Parser, schema, and real-Git tests for strict review plans (SPEC 7.3,
//! task T027).
//!
//! Every case proves one rule of the plan input contract:
//!
//! - the shipped parser fixtures parse or refuse exactly as their corpus
//!   directory states, and every refusal names its typed problem
//! - the shipped JSON Schema is the one the parser's field list generates,
//!   so no second validator can drift away from the Rust type
//! - a plan that satisfies the contract passes validation and reaches the
//!   pending dry-run slice, while an unknown field, a duplicate key, an
//!   invalid identifier, a missing decision, an overlap, or incomplete
//!   coverage is a typed invalid plan with unchanged effects
//! - a plan that names another comparison than the pinned review is
//!   `STALE_REVIEW`, including after the writer moved the candidate
//! - standard input carries a plan, and an oversized document is a typed
//!   limit rather than an unbounded read

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use margin::domain::{
    self, DecisionMode, PlanProblem, REVIEW_PLAN_FIELDS, REVIEW_PLAN_REQUIRED_FIELDS, ReviewPlan,
};
use serde_json::{Value, json};
use support::env::is_scrubbed;
use support::repo::TestRepo;
use support::text;

/// The fixture identities of the corpus: two well-formed change decisions.
const FIXTURE_CHANGE: &str = "sha1:5555555555555555555555555555555555555555";

/// Root of the repository, where the schema and the fixture corpus live.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

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
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Runs the installed executable with `document` on standard input.
fn margin_stdin(
    env: &support::env::IsolatedEnv,
    cwd: &Path,
    args: &[&str],
    document: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    child
        .stdin
        .as_mut()
        .expect("standard input is piped")
        .write_all(document)
        .expect("the plan document is written");
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot wait for the margin binary: {error}"))
}

/// The exit class of one finished binary run.
fn exit_of(output: &Output) -> i32 {
    output.status.code().unwrap_or_else(|| {
        panic!("the binary died from a signal");
    })
}

/// Parses the one JSON envelope of a machine-mode run (SPEC 7.1).
fn envelope_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["schema_version"], 1, "{what}");
    envelope
}

/// The typed refusal of one failed run: its exit class, error code, and
/// envelope.
fn refusal_of(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> (i32, String, Value) {
    let output = margin(env, cwd, args);
    let what = format!("{args:?}");
    let envelope = envelope_of(&output, &what);
    let code = envelope["error"]["code"].as_str().unwrap_or("").to_string();
    (exit_of(&output), code, envelope)
}

/// Writes one document next to the repository and returns its path, so no
/// plan file ever joins the scoped working tree.
fn plan_file(repo: &TestRepo, name: &str, document: &str) -> String {
    let path = repo.root().join(name);
    fs::write(&path, document)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    path.to_str().unwrap().to_string()
}

/// Initializes one workspace over `notes` and returns the start marker that
/// anchors the scope.
fn init_margin(repo: &TestRepo) -> String {
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "initialization must succeed");
    let envelope = envelope_of(&output, "init");
    assert_eq!(envelope["ok"], true, "init: {envelope}");
    text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// Writes one changed paragraph into a scoped file.
fn reparagraph(repo: &TestRepo, path: &str, words: &str) {
    repo.write_file(path, format!("# Title\n\n{words}\n\nClosing.\n").as_bytes());
}

/// Commits every change as one ordinary fixture commit and returns the new
/// `HEAD`.
fn commit(repo: &TestRepo, message: &str) -> String {
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", message]);
    text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// One workspace with two pending scoped changes: the JSON members of its
/// pinned context, the change identities of both groups, and the review
/// identity they belong to.
fn pending_review() -> (TestRepo, String, Vec<String>, String) {
    let repo = TestRepo::init();
    reparagraph(&repo, "notes/a.md", "Original a.");
    reparagraph(&repo, "notes/b.md", "Original b.");
    let baseline = init_margin(&repo);
    reparagraph(&repo, "notes/a.md", "Changed a.");
    reparagraph(&repo, "notes/b.md", "Changed b.");
    let candidate = commit(&repo, "writing pass");

    let diff = margin(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!(exit_of(&diff), 0, "the pinned review builds");
    let envelope = envelope_of(&diff, "diff");
    assert_eq!(envelope["ok"], true, "diff: {envelope}");
    let data = &envelope["data"];
    assert_eq!(data["baseline_oid"], baseline);
    assert_eq!(data["candidate_oid"], candidate);
    let changes: Vec<String> = data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["change_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(changes.len(), 2, "one group per edited file");
    let review_id = data["review_id"].as_str().unwrap().to_string();

    let context = json!({
        "schema_version": 1,
        "request_id": "review-introduction-01",
        "review_id": data["review_id"],
        "branch_ref": data["branch_ref"],
        "scope_id": data["scope_id"],
        "baseline_oid": data["baseline_oid"],
        "candidate_oid": data["candidate_oid"],
    })
    .to_string();
    (repo, context, changes, review_id)
}

/// One complete plan document: the pinned context plus the decision members
/// of this test. Extra members beyond the two decision fields exercise the
/// strict field set.
fn plan_of(context: &str, members: &str) -> String {
    format!("{}, {members}}}", &context[..context.len() - 1])
}

/// The typed problem one invalid fixture must refuse with. The two variants
/// whose text repeats implementation wording are compared by variant and
/// public code only.
fn expected_problem(stem: &str) -> PlanProblem {
    match stem {
        "duplicate-decision" => PlanProblem::DuplicateDecision {
            id: FIXTURE_CHANGE.to_string(),
        },
        "duplicate-key" => PlanProblem::DuplicateField {
            field: "restore".to_string(),
        },
        "false-without-keep" | "no-decision" => PlanProblem::MissingDecision,
        "missing-request-id" => PlanProblem::MissingField {
            field: "request_id",
        },
        "missing-restore" => PlanProblem::MissingField { field: "restore" },
        "missing-review-id" => PlanProblem::MissingField { field: "review_id" },
        "missing-schema-version" => PlanProblem::MissingField {
            field: "schema_version",
        },
        "not-an-object" | "truncated" => PlanProblem::NotJson { why: String::new() },
        "overlapping-decision" => PlanProblem::OverlappingDecision {
            id: FIXTURE_CHANGE.to_string(),
        },
        "oversized-message" => PlanProblem::Message {
            problem: domain::MetadataProblem::OversizedMessage { bytes: 4097 },
        },
        "trailer-injection" => PlanProblem::Message {
            problem: domain::MetadataProblem::ReservedTrailerInjection {
                line: "Margin-Kind: restore".to_string(),
            },
        },
        "two-decisions" => PlanProblem::ConflictingDecision,
        "unknown-field" => PlanProblem::UnknownField {
            field: "approved_by".to_string(),
        },
        "unsupported-version" => PlanProblem::UnsupportedVersion { observed: 2 },
        "bad-baseline-oid"
        | "bad-branch-ref"
        | "bad-candidate-oid"
        | "bad-change-id"
        | "bad-request-id"
        | "bad-request-id-charset"
        | "bad-review-id"
        | "bad-scope-id"
        | "long-request-id" => PlanProblem::InvalidValue {
            field: "",
            why: String::new(),
        },
        "non-string-in-keep"
        | "null-message"
        | "wrong-type-keep-remaining"
        | "wrong-type-restore"
        | "wrong-type-schema-version" => PlanProblem::WrongShape {
            field: "",
            expected: "",
        },
        other => panic!("no expected problem is recorded for the fixture {other}"),
    }
}

/// The variant name of one problem, so a failure prints the difference.
fn kind_of(problem: &PlanProblem) -> &'static str {
    match problem {
        PlanProblem::NotJson { .. } => "NotJson",
        PlanProblem::UnknownField { .. } => "UnknownField",
        PlanProblem::DuplicateField { .. } => "DuplicateField",
        PlanProblem::MissingField { .. } => "MissingField",
        PlanProblem::WrongShape { .. } => "WrongShape",
        PlanProblem::UnsupportedVersion { .. } => "UnsupportedVersion",
        PlanProblem::InvalidValue { .. } => "InvalidValue",
        PlanProblem::MissingDecision => "MissingDecision",
        PlanProblem::ConflictingDecision => "ConflictingDecision",
        PlanProblem::DuplicateDecision { .. } => "DuplicateDecision",
        PlanProblem::OverlappingDecision { .. } => "OverlappingDecision",
        PlanProblem::Message { .. } => "Message",
        PlanProblem::UnknownChange { .. } => "UnknownChange",
        PlanProblem::IncompleteCoverage { .. } => "IncompleteCoverage",
    }
}

/// The fixtures compared by variant only, because their diagnostic text
/// repeats the wording of the implementation.
fn compared_by_kind(stem: &str) -> bool {
    matches!(
        expected_problem(stem),
        PlanProblem::InvalidValue { .. }
            | PlanProblem::WrongShape { .. }
            | PlanProblem::NotJson { .. }
    )
}

#[test]
fn the_shipped_schema_is_the_one_the_parser_generates() {
    let shipped = fs::read_to_string(project_root().join("schemas/review-plan-v1.json"))
        .expect("the release ships the review plan schema");
    assert_eq!(shipped, domain::review_plan_json_schema());

    // The schema describes exactly the fields the strict parser reads, and
    // it closes the object to unknown members the same way (SPEC 7.3).
    let schema: Value = serde_json::from_str(&shipped).unwrap();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let mut described: Vec<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    described.sort_unstable();
    let mut defined: Vec<&str> = REVIEW_PLAN_FIELDS.to_vec();
    defined.sort_unstable();
    assert_eq!(described, defined, "the schema names every parser field");
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(required, REVIEW_PLAN_REQUIRED_FIELDS);

    // The decision modes stay mutually exclusive in the schema too: the
    // one-of branches are stated, not a free-form object.
    let branches = schema["oneOf"].as_array().unwrap();
    assert_eq!(branches.len(), 2, "one branch per decision mode");
}

#[test]
fn parser_fixtures_parse_or_refuse_exactly_as_recorded() {
    let valid = project_root().join("tests/fixtures/plans/valid");
    let mut accepted = 0;
    for entry in fs::read_dir(&valid).expect("the valid corpus exists") {
        let path = entry.expect("the entry reads").path();
        let raw = fs::read(&path).expect("the fixture reads");
        let plan = ReviewPlan::parse(&raw)
            .unwrap_or_else(|problem| panic!("{} must parse: {problem}", path.display()));
        assert!(!plan.request_id.is_empty(), "{}", path.display());
        assert!(!plan.message.is_empty(), "the effective message resolves");
        let decided: Vec<&String> = plan
            .restore
            .iter()
            .chain(match &plan.decision_mode {
                DecisionMode::KeepRemaining => [].as_slice(),
                DecisionMode::KeepExplicit { keep } => keep.as_slice(),
            })
            .collect();
        assert!(
            decided.iter().all(|id| id.starts_with("sha1:")),
            "{}: every decision is one full identity",
            path.display()
        );
        accepted += 1;
    }
    assert!(accepted >= 5, "the corpus holds several valid documents");

    let invalid = project_root().join("tests/fixtures/plans/invalid");
    let mut refused = 0;
    for entry in fs::read_dir(&invalid).expect("the invalid corpus exists") {
        let path = entry.expect("the entry reads").path();
        let raw = fs::read(&path).expect("the fixture reads");
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let expected = expected_problem(&stem);
        let Err(problem) = ReviewPlan::parse(&raw) else {
            panic!("{} must refuse", path.display());
        };
        assert_eq!(
            kind_of(&problem),
            kind_of(&expected),
            "{}: {problem}",
            path.display()
        );
        assert_eq!(problem.public_code(), expected.public_code());
        if !compared_by_kind(&stem) {
            assert_eq!(problem, expected, "{}", path.display());
        }
        assert!(
            !problem.to_string().is_empty(),
            "the problem explains itself"
        );
        refused += 1;
    }
    assert!(refused >= 25, "the corpus holds many broken documents");
}

#[test]
fn a_valid_plan_passes_validation_and_reports_its_intended_result() {
    let (repo, context, changes, _review_id) = pending_review();

    // The plan satisfies every rule of the contract, so the dry run
    // succeeds, reports the decisions it validated, and writes nothing
    // (SPEC 6.4, task T029).
    let restore_all = plan_of(
        &context,
        &format!(
            r#""restore": [{:?}, {:?}], "keep_remaining": true"#,
            changes[0], changes[1]
        ),
    );
    let plan = plan_file(&repo, "plan.json", &restore_all);
    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "a valid plan");
    assert_eq!(exit_of(&output), 0, "the dry run validates the plan");
    assert_eq!(envelope["ok"], true);
    let data = &envelope["data"];
    assert_eq!(data["dry_run"], true);
    assert_eq!(data["candidate_oid"], context_candidate(&context));
    assert_eq!(
        data["restored"].as_array().unwrap().len(),
        2,
        "both decisions validated"
    );
    assert_eq!(data["kept"], 0);
    // Restoring every group recomposes the baseline's scoped entries, and
    // nothing outside the scope changed between B and C, so the intended
    // tree is the baseline tree itself — checked with stock Git.
    let baseline_tree = text(&repo.git_ok(["rev-parse", "HEAD~1^{tree}"]));
    assert_eq!(data["expected_tree"], baseline_tree);
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");

    // An explicit keep array that names every other change passes too, and
    // an empty restore with `keep_remaining` approves everything.
    let explicit = plan_of(
        &context,
        &format!(
            r#""restore": [{:?}], "keep": [{:?}], "message": "Keep the tone""#,
            changes[0], changes[1]
        ),
    );
    let plan = plan_file(&repo, "plan-explicit.json", &explicit);
    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "an explicit keep array");
    assert_eq!(exit_of(&output), 0);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["kept"], 1);
    assert_eq!(envelope["data"]["message"], "Keep the tone");

    // Nothing the validation path does reaches the repository.
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let status = margin(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&status, "status after validation");
    assert_eq!(
        envelope["data"]["head"], head,
        "the candidate stays the pinned one"
    );
}

/// The candidate object ID of one pinned context document.
fn context_candidate(context: &str) -> String {
    let value: Value = serde_json::from_str(context).unwrap();
    value["candidate_oid"].as_str().unwrap().to_string()
}

#[test]
fn structural_faults_are_typed_invalid_plans() {
    let (repo, context, changes, _review_id) = pending_review();
    let broken: Vec<(&str, String)> = vec![
        (
            "an unknown field",
            plan_of(
                &context,
                r#""restore": [], "keep_remaining": true, "approved_by": "the agent""#,
            ),
        ),
        (
            "a duplicate key",
            plan_of(
                &context,
                &format!(
                    r#""restore": [{:?}], "restore": [{:?}], "keep_remaining": true"#,
                    changes[0], changes[0]
                ),
            ),
        ),
        (
            "a bad request identifier",
            plan_of(
                &context,
                r#""restore": [], "keep_remaining": true, "request_id": "review introduction""#,
            ),
        ),
        (
            "a bad review identity",
            plan_of(
                &context,
                r#""restore": [], "keep_remaining": true, "review_id": "review-1""#,
            ),
        ),
        ("a missing decision", plan_of(&context, r#""restore": []"#)),
        (
            "a false flag without a keep array",
            plan_of(&context, r#""restore": [], "keep_remaining": false"#),
        ),
        (
            "two decision modes",
            plan_of(
                &context,
                r#""restore": [], "keep_remaining": true, "keep": []"#,
            ),
        ),
        (
            "a repeated decision",
            plan_of(
                &context,
                &format!(
                    r#""restore": [{:?}, {:?}], "keep_remaining": true"#,
                    changes[0], changes[0]
                ),
            ),
        ),
        (
            "an overlapping decision",
            plan_of(
                &context,
                &format!(
                    r#""restore": [{:?}], "keep": [{:?}]"#,
                    changes[0], changes[0]
                ),
            ),
        ),
        (
            "a trailer injection",
            plan_of(
                &context,
                r#""restore": [], "keep_remaining": true, "message": "Keep it\nMargin-Kind: restore""#,
            ),
        ),
    ];
    assert_eq!(
        broken.len(),
        10,
        "each structural rule of the contract has one case"
    );
    for (what, document) in broken {
        let plan = plan_file(&repo, "broken.json", &document);
        let (exit, code, envelope) = refusal_of(
            repo.env(),
            repo.path(),
            &["review", "--input", &plan, "--dry-run", "--json"],
        );
        assert_eq!(exit, 2, "{what}: an invalid plan is invalid input");
        assert_eq!(code, "INVALID_PLAN", "{what}: {envelope}");
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(!message.is_empty(), "{what}: the refusal explains itself");
        assert_eq!(envelope["effects"]["working_tree"], "unchanged", "{what}");
        assert_eq!(envelope["effects"]["commit"], "none", "{what}");
    }
}

#[test]
fn unknown_and_undecided_changes_are_named() {
    let (repo, context, changes, _review_id) = pending_review();
    let stranger = changes[0].replace('0', "f");

    // An identity the pinned review never issued.
    let unknown = plan_of(
        &context,
        &format!(r#""restore": [{stranger:?}], "keep_remaining": true"#),
    );
    let plan = plan_file(&repo, "unknown.json", &unknown);
    let (exit, code, envelope) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 2);
    assert_eq!(code, "UNKNOWN_CHANGE", "{envelope}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&stranger),
        "the message names the identity"
    );

    // A keep array that leaves one change undecided.
    let incomplete = plan_of(
        &context,
        &format!(r#""restore": [], "keep": [{:?}]"#, changes[0]),
    );
    let plan = plan_file(&repo, "incomplete.json", &incomplete);
    let (exit, code, envelope) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 2);
    assert_eq!(code, "INVALID_PLAN", "{envelope}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("1 change undecided"),
        "the message counts the undecided change: {message}"
    );
    assert!(message.contains(&changes[1]), "the message names it");

    // A well-formed but empty decision set against a review with changes
    // is incomplete coverage too, never a silent approval of nothing.
    let empty = plan_of(&context, r#""restore": [], "keep": []"#);
    let plan = plan_file(&repo, "empty.json", &empty);
    let (_, code, _) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(code, "INVALID_PLAN");
}

#[test]
fn a_plan_of_another_comparison_is_stale() {
    let (repo, context, changes, review_id) = pending_review();

    // One context field that names another review.
    let stale = plan_of(
        &context.replace(&review_id, &changes[0]),
        &format!(r#""restore": [{:?}], "keep_remaining": true"#, changes[0]),
    );
    let plan = plan_file(&repo, "stale.json", &stale);
    let (exit, code, envelope) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 3, "a stale plan is conflicting context");
    assert_eq!(code, "STALE_REVIEW", "{envelope}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.contains("review_id"), "the message names the field");

    // The same plan after the writer moved the candidate: the pinned
    // comparison changed underneath a document that still names the old
    // one, so the plan is stale rather than silently reused.
    let valid = plan_of(
        &context,
        &format!(r#""restore": [{:?}], "keep_remaining": true"#, changes[0]),
    );
    let plan = plan_file(&repo, "moved.json", &valid);
    reparagraph(&repo, "notes/a.md", "Changed again.");
    commit(&repo, "another writing pass");
    let (exit, code, _) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 3);
    assert_eq!(code, "STALE_REVIEW");
}

#[test]
fn standard_input_carries_one_plan() {
    let (repo, context, _changes, _review_id) = pending_review();

    // A field with the wrong JSON shape.
    let invalid = plan_of(&context, r#""restore": [], "keep_remaining": "yes""#);
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["review", "--input", "-", "--dry-run", "--json"],
        invalid.as_bytes(),
    );
    let envelope = envelope_of(&output, "a plan on standard input");
    assert_eq!(exit_of(&output), 2);
    assert_eq!(envelope["error"]["code"], "INVALID_PLAN", "{envelope}");

    // A document that is not JSON at all refuses the same way.
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["review", "--input", "-", "--dry-run", "--json"],
        b"{not json",
    );
    let envelope = envelope_of(&output, "broken bytes on standard input");
    assert_eq!(exit_of(&output), 2);
    assert_eq!(envelope["error"]["code"], "INVALID_PLAN", "{envelope}");

    // Invalid UTF-8 refuses before any identity is read.
    let mut raw = plan_of(&context, r#""restore": [], "keep_remaining": true"#).into_bytes();
    raw.insert(0, 0xff);
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["review", "--input", "-", "--dry-run", "--json"],
        &raw,
    );
    let envelope = envelope_of(&output, "invalid UTF-8 on standard input");
    assert_eq!(exit_of(&output), 2);
    assert_eq!(envelope["error"]["code"], "INVALID_PLAN", "{envelope}");
}

#[test]
fn an_oversized_plan_document_is_a_typed_limit() {
    let (repo, context, _changes, _review_id) = pending_review();
    let padding = "K".repeat(domain::MAX_PLAN_BYTES + 2);
    let oversized = plan_of(
        &context,
        &format!(r#""restore": [], "keep_remaining": true, "message": "{padding}""#),
    );
    assert!(oversized.len() > domain::MAX_PLAN_BYTES);
    let plan = plan_file(&repo, "huge.json", &oversized);
    let (exit, code, envelope) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 2);
    assert_eq!(code, "LIMIT_EXCEEDED", "{envelope}");
}

#[test]
fn the_plan_rules_are_reachable_through_the_library() {
    let (_repo, context, changes, _review_id) = pending_review();
    let known: BTreeSet<String> = changes.iter().map(String::to_string).collect();

    // The same rules the binary enforces are reachable through the library,
    // so the TUI and the hooks can share them (SPEC 13.6).
    let restore_all = ReviewPlan::parse(
        plan_of(
            &context,
            &format!(
                r#""restore": [{:?}, {:?}], "keep_remaining": true"#,
                changes[0], changes[1]
            ),
        )
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(domain::validate_plan_coverage(&restore_all, &known), Ok(()));

    let keep_one = ReviewPlan::parse(
        plan_of(
            &context,
            &format!(
                r#""restore": [{:?}], "keep": [{:?}]"#,
                changes[1], changes[0]
            ),
        )
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        keep_one.decision_mode,
        DecisionMode::KeepExplicit {
            keep: vec![changes[0].clone()]
        }
    );
    assert_eq!(domain::validate_plan_coverage(&keep_one, &known), Ok(()));

    let stranger = changes[0].replace('0', "f");
    let unknown = ReviewPlan::parse(
        plan_of(
            &context,
            &format!(r#""restore": [{stranger:?}], "keep_remaining": true"#),
        )
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        domain::validate_plan_coverage(&unknown, &known),
        Err(PlanProblem::UnknownChange { id: stranger })
    );
}

#[test]
fn a_broken_document_refuses_typed_before_the_application() {
    let (repo, context, _changes, _review_id) = pending_review();
    let valid = plan_of(&context, r#""restore": [], "keep_remaining": true"#);
    let broken = plan_of(&context, r#""restore": []"#);

    // A document no rule accepts never reads as a missing feature.
    let plan = plan_file(&repo, "apply-broken.json", &broken);
    let (exit, code, envelope) = refusal_of(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--json"],
    );
    assert_eq!(exit, 2);
    assert_eq!(code, "INVALID_PLAN", "{envelope}");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");

    // A well-formed keep-all plan reaches the application and commits it
    // (SPEC 9.4, T031), and the resumption of that same request reports
    // the receipt the history already holds: the receipt lookup of SPEC 8.7
    // runs before any stale check, so the completed decision never
    // commits twice (SPEC 9.7, T035).
    let plan = plan_file(&repo, "apply.json", &valid);
    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the application commits");
    let envelope = envelope_of(&output, "apply");
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(
        envelope["data"]["commit"],
        text(&repo.git_ok(["rev-parse", "HEAD"])),
        "the response names the review commit"
    );
    assert_eq!(envelope["effects"]["commit"], "created");

    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--resume", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the resumption reports the receipt");
    let envelope = envelope_of(&output, "resume");
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["effects"]["commit"], "existing_receipt");
    assert_eq!(
        envelope["data"]["existing_receipt"],
        text(&repo.git_ok(["rev-parse", "HEAD"])),
        "the resumption names the receipt commit"
    );
    assert_eq!(
        envelope["data"]["commit"],
        Value::Null,
        "no second review commit was made: {envelope}"
    );
}
