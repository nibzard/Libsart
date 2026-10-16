//! Real-Git tests for the canonical identities (SPEC 8.6, task T011).
//!
//! Every case proves one identity rule against the installed Git:
//!
//! - each identity is Git's own object ID of domain-separated canonical bytes
//! - fixed inputs produce fixed IDs
//! - a changed contract input changes the applicable identity
//! - arrays are sorted where order is not semantic and ordered where it is
//! - working content hashes raw bytes with mode and presence
//! - SHA-1 and SHA-256 repositories both work and qualify their identifiers
//! - an entry that cannot honestly join a digest refuses with a typed error
//!
//! The oracles rebuild the documented canonical byte format in this file,
//! independently of the encoder under test, and ask stock Git for the object
//! ID (SPEC 16.3).

mod support;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;

use margin::app::{
    Identities, IdentityError, ResolvedScope, resolve_scope, scoped_content_digest,
    working_content_digest,
};
use margin::domain::{
    self, ChangeGroup, ContentEntry, DecisionMode, ENGINE_VERSION, EntryMode, PlanIdentity,
    ReviewContext, ScopeInput,
};
use margin::git::Git;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};

/// Blob object ID of `alpha\n`, pinned by the SHA-1 contract.
const GOLDEN_ALPHA: &str = "4a58007052a65fbc2fc3f910f2855f45a4058e74";
/// Blob object ID of `beta\n`, pinned by the SHA-1 contract.
const GOLDEN_BETA: &str = "65b2df87f7df3aeedef04be96703e55ac19c2cfb";
/// The content digest of two fixed entries under the v1 encoding. Any change
/// to the canonical byte format of an identity fails this suite.
const GOLDEN_CONTENT: &str = "761730efee5bee9ebeb6c0c60207eff4b0285893";

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Resolves one scope or panics with the error.
fn resolve(repo: &TestRepo, include: &[&str], suffixes: &[&str]) -> ResolvedScope {
    let input = ScopeInput {
        include: include.iter().map(|root| root.to_string()).collect(),
        exclude: Vec::new(),
        suffixes: suffixes.iter().map(|suffix| suffix.to_string()).collect(),
    };
    resolve_scope(&adapter_for(repo), &input)
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
}

/// The stock-Git object ID of `bytes`, computed outside Margin.
fn hash_object_oracle(repo: &TestRepo, bytes: &[u8]) -> String {
    support::text(&repo.git_stdin(["hash-object", "-t", "blob", "--stdin"], bytes))
}

/// The full commit object ID of `HEAD`, from stock Git.
fn head_oid(repo: &TestRepo) -> String {
    support::text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// Writes one length-prefixed canonical value, mirroring the documented
/// format independently of the encoder under test.
fn put_value(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value);
}

/// Writes one canonical integer field.
fn put_count(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

/// Rebuilds the canonical content-digest input from raw parts.
fn expected_content_input(entries: &[(&str, EntryMode, &str)]) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut bytes = domain::CONTENT_DIGEST_DOMAIN.to_vec();
    put_count(&mut bytes, sorted.len() as u64);
    for (path, mode, oid) in sorted {
        put_value(&mut bytes, path.as_bytes());
        put_value(&mut bytes, mode.as_str().as_bytes());
        put_value(&mut bytes, oid.as_bytes());
    }
    bytes
}

fn content_entry(path: &str, mode: EntryMode, blob_oid: &str) -> ContentEntry {
    ContentEntry {
        path: path.to_string(),
        mode,
        blob_oid: blob_oid.to_string(),
    }
}

#[test]
fn content_id_matches_stock_git_and_pins_the_v1_encoding() {
    let repo = TestRepo::init();
    let adapter = adapter_for(&repo);
    let ids = Identities::new(&adapter).expect("the identities load");

    // The environment's Git reproduces the pinned blob IDs.
    assert_eq!(hash_object_oracle(&repo, b"alpha\n"), GOLDEN_ALPHA);
    assert_eq!(hash_object_oracle(&repo, b"beta\n"), GOLDEN_BETA);

    let entries = vec![
        content_entry("b.md", EntryMode::Regular, GOLDEN_BETA),
        content_entry("a.md", EntryMode::Executable, GOLDEN_ALPHA),
    ];
    let fingerprint = ids.content(&entries).expect("the content digest computes");
    // Fixed inputs produce a fixed ID, whatever order the entries arrive in.
    assert_eq!(ids.content(&entries).unwrap(), fingerprint);
    let mut shuffled = entries.clone();
    shuffled.reverse();
    assert_eq!(ids.content(&shuffled).unwrap(), fingerprint);
    // The ID is stock Git's object ID of the documented canonical bytes.
    assert_eq!(
        fingerprint.oid,
        hash_object_oracle(
            &repo,
            &expected_content_input(&[
                ("a.md", EntryMode::Executable, GOLDEN_ALPHA),
                ("b.md", EntryMode::Regular, GOLDEN_BETA)
            ])
        )
    );
    // The pinned golden value guards the byte format itself.
    assert_eq!(fingerprint.oid, GOLDEN_CONTENT);
    assert_eq!(fingerprint.id(), format!("sha1:{GOLDEN_CONTENT}"));
}

#[test]
fn content_id_changes_with_every_entry_and_with_absence() {
    let repo = TestRepo::init();
    let adapter = adapter_for(&repo);
    let ids = Identities::new(&adapter).expect("the identities load");

    let base = ids
        .content(&[content_entry("a.md", EntryMode::Regular, GOLDEN_ALPHA)])
        .unwrap();
    // Path, mode, and blob identity each change the digest.
    assert_ne!(
        ids.content(&[content_entry("b.md", EntryMode::Regular, GOLDEN_ALPHA)])
            .unwrap(),
        base
    );
    assert_ne!(
        ids.content(&[content_entry("a.md", EntryMode::Executable, GOLDEN_ALPHA)])
            .unwrap(),
        base
    );
    assert_ne!(
        ids.content(&[content_entry("a.md", EntryMode::Regular, GOLDEN_BETA)])
            .unwrap(),
        base
    );
    // Presence is the entry set: a second entry and an empty set both differ.
    assert_ne!(
        ids.content(&[
            content_entry("a.md", EntryMode::Regular, GOLDEN_ALPHA),
            content_entry("z.md", EntryMode::Regular, GOLDEN_BETA)
        ])
        .unwrap(),
        base
    );
    assert_ne!(ids.content(&[]).unwrap(), base);
    // Equal content under a different path identity stays distinct.
    assert_ne!(
        ids.content(&[content_entry("a.md", EntryMode::Regular, GOLDEN_ALPHA)])
            .unwrap(),
        ids.content(&[content_entry("copy.md", EntryMode::Regular, GOLDEN_ALPHA)])
            .unwrap()
    );
}

#[test]
fn working_digest_hashes_raw_bytes_with_mode_and_presence() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"alpha\n");
    repo.write_file("content/new\nline.md", "odd name\n".as_bytes());
    repo.write_file("content/opaque.md", b"\xff\xfe\x00 raw bytes");
    repo.commit_all("seed");
    let resolved = resolve(&repo, &["content/"], &[".md"]);
    let adapter = adapter_for(&repo);

    let digest = working_content_digest(&adapter, &resolved).expect("the digest computes");
    // Fixed inputs produce a fixed ID.
    assert_eq!(working_content_digest(&adapter, &resolved).unwrap(), digest);

    // The oracle rebuilds the input from the raw bytes of each file, proving
    // the digest hashes working bytes, not index state.
    let mut oracle_entries: Vec<(String, EntryMode, String)> = Vec::new();
    for name in ["content/a.md", "content/new\nline.md", "content/opaque.md"] {
        let raw = repo.read_file(name);
        let oid = hash_object_oracle(&repo, &raw);
        oracle_entries.push((name.to_string(), EntryMode::Regular, oid));
    }
    let oracle_refs: Vec<(&str, EntryMode, &str)> = oracle_entries
        .iter()
        .map(|(path, mode, oid)| (path.as_str(), *mode, oid.as_str()))
        .collect();
    assert_eq!(
        digest.oid,
        hash_object_oracle(&repo, &expected_content_input(&oracle_refs))
    );

    // Changing one raw byte changes the digest; restoring it returns to the
    // same value.
    repo.write_file("content/a.md", b"alpha!\n");
    let edited = working_content_digest(&adapter, &resolved).unwrap();
    assert_ne!(edited, digest);
    repo.write_file("content/a.md", b"alpha\n");
    assert_eq!(working_content_digest(&adapter, &resolved).unwrap(), digest);

    // Git records only the owner executable bit.
    repo.set_mode("content/a.md", 0o744);
    assert_ne!(working_content_digest(&adapter, &resolved).unwrap(), digest);
    repo.set_mode("content/a.md", 0o610);
    assert_eq!(working_content_digest(&adapter, &resolved).unwrap(), digest);
    repo.set_mode("content/a.md", 0o644);

    // A deleted working file leaves the entry set: absence changes the
    // digest, and identical restored content brings the original back.
    fs::remove_file(repo.file_path("content/a.md")).expect("the file is removed");
    let deleted = working_content_digest(&adapter, &resolve(&repo, &["content/"], &[".md"]))
        .expect("the digest still computes");
    assert_ne!(deleted, digest);
    repo.write_file("content/a.md", b"alpha\n");
    assert_eq!(
        working_content_digest(&adapter, &resolve(&repo, &["content/"], &[".md"])).unwrap(),
        digest
    );
}

#[test]
fn batched_working_hashes_match_single_blob_hashes() {
    let repo = TestRepo::init();
    // More files than one batch, with a name that stresses argv.
    let mut paths = Vec::new();
    for index in 0..300 {
        let name = if index == 299 {
            "content/odd name\n299.md".to_string()
        } else {
            format!("content/doc-{index:03}.md")
        };
        repo.write_file(&name, format!("body {index}\n").as_bytes());
        paths.push(OsString::from(name));
    }
    repo.commit_all("seed");
    let adapter = adapter_for(&repo);

    let batched = adapter.hash_object_paths(&paths).expect("the batch hashes");
    assert_eq!(batched.len(), paths.len());
    for (path, oid) in paths.iter().zip(&batched) {
        let raw = repo.read_file(path.to_str().expect("test paths are UTF-8"));
        assert_eq!(
            oid,
            &hash_object_oracle(&repo, &raw),
            "raw bytes of {path:?}"
        );
    }
    // An empty list runs no process and answers no hashes.
    assert_eq!(
        adapter.hash_object_paths(&[]).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn scoped_content_digest_follows_the_scope_not_the_whole_tree() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"one\n");
    repo.write_file("notes.txt", b"outside\n");
    repo.commit_all("seed");
    let resolved = resolve(&repo, &["content/"], &[".md"]);
    let adapter = adapter_for(&repo);

    let digest =
        scoped_content_digest(&adapter, &head_oid(&repo), &resolved).expect("the digest computes");
    assert_eq!(
        scoped_content_digest(&adapter, &head_oid(&repo), &resolved).unwrap(),
        digest
    );

    // Out-of-scope content never reaches the digest.
    repo.write_file("notes.txt", b"outside changed\n");
    repo.write_file("more/other.txt", b"also outside\n");
    repo.commit_all("outside edits");
    assert_eq!(
        scoped_content_digest(&adapter, &head_oid(&repo), &resolved).unwrap(),
        digest
    );

    // Scoped content does.
    repo.write_file("content/a.md", b"two\n");
    repo.commit_all("scoped edit");
    let changed = scoped_content_digest(&adapter, &head_oid(&repo), &resolved).unwrap();
    assert_ne!(changed, digest);

    // The digest follows the tree, not the commit: an empty commit with the
    // same tree gives the same digest.
    repo.git_ok(["commit", "--quiet", "--allow-empty", "--message", "empty"]);
    assert_eq!(
        scoped_content_digest(&adapter, &head_oid(&repo), &resolved).unwrap(),
        changed
    );
}

#[test]
fn review_change_and_plan_ids_match_stock_git_bytes() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"one\ntwo\n");
    let base = repo.commit_all("base");
    repo.write_file("content/a.md", b"one\nTWO\n");
    let tip = repo.commit_all("tip");
    // Real patch bytes from a fixed diff contract (SPEC 10.1).
    let patch = repo.git_ok([
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--full-index",
        &base,
        &tip,
    ]);
    assert!(!patch.is_empty());

    let adapter = adapter_for(&repo);
    let ids = Identities::new(&adapter).expect("the identities load");
    let resolved = resolve(&repo, &["content/"], &[".md"]);
    let groups = vec![ChangeGroup {
        path: "content/a.md".to_string(),
        patch_units: vec![patch.clone()],
    }];

    // The manifest digest covers groups, paths, patch bytes, and ordering.
    let manifest = ids.manifest(&groups).expect("the manifest computes");
    let mut expected_manifest = domain::MANIFEST_DIGEST_DOMAIN.to_vec();
    put_count(&mut expected_manifest, 1);
    put_value(&mut expected_manifest, b"content/a.md");
    put_count(&mut expected_manifest, 1);
    put_value(&mut expected_manifest, &patch);
    assert_eq!(manifest.oid, hash_object_oracle(&repo, &expected_manifest));
    // Group ordering is semantic.
    let second = ChangeGroup {
        path: "content/b.md".to_string(),
        patch_units: vec![patch.clone()],
    };
    assert_ne!(
        ids.manifest(&[groups[0].clone(), second.clone()]).unwrap(),
        ids.manifest(&[second, groups[0].clone()]).unwrap()
    );

    // The review identity covers branch, B, C, scope, engine, Git version,
    // and the manifest digest.
    let branch_ref = "refs/heads/main".to_string();
    let context = ReviewContext {
        branch_ref: branch_ref.clone(),
        baseline_oid: base.clone(),
        candidate_oid: tip.clone(),
        scope: resolved.descriptor.clone(),
        engine_version: ENGINE_VERSION.to_string(),
        git_version: adapter.git_version().expect("git reports its version"),
        manifest_digest: manifest.oid.clone(),
    };
    let review = ids.review(&context).expect("the review computes");
    let mut expected_review = domain::REVIEW_ID_DOMAIN.to_vec();
    put_value(&mut expected_review, context.branch_ref.as_bytes());
    put_value(&mut expected_review, context.baseline_oid.as_bytes());
    put_value(&mut expected_review, context.candidate_oid.as_bytes());
    put_value(&mut expected_review, &resolved.descriptor.canonical_bytes());
    put_value(&mut expected_review, context.engine_version.as_bytes());
    put_value(&mut expected_review, context.git_version.as_bytes());
    put_value(&mut expected_review, context.manifest_digest.as_bytes());
    assert_eq!(review.oid, hash_object_oracle(&repo, &expected_review));
    assert_eq!(ids.review(&context).unwrap(), review);

    // A change identity covers its review, path, patch units, and ordering.
    let changes = ids
        .changes(&review, &groups)
        .expect("the change identities compute");
    assert_eq!(changes.len(), 1);
    let mut expected_change = domain::CHANGE_ID_DOMAIN.to_vec();
    put_value(&mut expected_change, review.oid.as_bytes());
    put_value(&mut expected_change, b"content/a.md");
    put_count(&mut expected_change, 1);
    put_value(&mut expected_change, &patch);
    put_count(&mut expected_change, 0);
    assert_eq!(changes[0].oid, hash_object_oracle(&repo, &expected_change));
    // A changed contract input regenerates the change identities: the same
    // groups under a different review get different change IDs.
    let other_review = ids
        .review(&ReviewContext {
            git_version: "0.0.0-test".to_string(),
            ..context.clone()
        })
        .unwrap();
    let other_changes = ids.changes(&other_review, &groups).unwrap();
    assert_ne!(changes, other_changes);

    // The plan fingerprint covers the operation kind, every context field,
    // the sorted decisions, the decision mode, and the effective message.
    let plan = PlanIdentity {
        operation: "review".to_string(),
        request_id: "review-introduction-01".to_string(),
        review_id: review.id(),
        branch_ref,
        scope_id: resolved.digest.id(),
        baseline_oid: base,
        candidate_oid: tip,
        restore: vec![changes[0].id()],
        decision_mode: DecisionMode::KeepRemaining,
        message: "Keep the shorter opening".to_string(),
    };
    let fingerprint = ids.plan(&plan).expect("the plan computes");
    let mut expected_plan = domain::PLAN_FINGERPRINT_DOMAIN.to_vec();
    put_value(&mut expected_plan, b"review");
    put_value(&mut expected_plan, b"review-introduction-01");
    put_value(&mut expected_plan, plan.review_id.as_bytes());
    put_value(&mut expected_plan, plan.branch_ref.as_bytes());
    put_value(&mut expected_plan, plan.scope_id.as_bytes());
    put_value(&mut expected_plan, plan.baseline_oid.as_bytes());
    put_value(&mut expected_plan, plan.candidate_oid.as_bytes());
    put_count(&mut expected_plan, 1);
    put_value(&mut expected_plan, changes[0].id().as_bytes());
    expected_plan.push(0);
    put_value(&mut expected_plan, plan.message.as_bytes());
    assert_eq!(fingerprint.oid, hash_object_oracle(&repo, &expected_plan));
    assert_eq!(ids.plan(&plan).unwrap(), fingerprint);
}

#[test]
fn identities_are_domain_separated() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"alpha\n");
    repo.commit_all("seed");
    let adapter = adapter_for(&repo);
    let ids = Identities::new(&adapter).expect("the identities load");
    let resolved = resolve(&repo, &["content/"], &[".md"]);

    // No two identity kinds share a domain prefix.
    let domains = [
        domain::SCOPE_DIGEST_DOMAIN,
        domain::CONTENT_DIGEST_DOMAIN,
        domain::MANIFEST_DIGEST_DOMAIN,
        domain::REVIEW_ID_DOMAIN,
        domain::CHANGE_ID_DOMAIN,
        domain::PLAN_FINGERPRINT_DOMAIN,
    ];
    let mut prefixes = BTreeSet::new();
    for domain in domains {
        assert!(prefixes.insert(domain.to_vec()), "the prefixes differ");
    }

    // Inputs that share every field value still give six distinct
    // identifiers, because the domain prefix is part of the hashed bytes.
    let blob = hash_object_oracle(&repo, b"alpha\n");
    let group = ChangeGroup {
        path: "content/a.md".to_string(),
        patch_units: vec![blob.as_bytes().to_vec()],
    };
    let manifest = ids.manifest(std::slice::from_ref(&group)).unwrap();
    let context = ReviewContext {
        branch_ref: format!("refs/heads/{}", "main"),
        baseline_oid: blob.clone(),
        candidate_oid: blob.clone(),
        scope: resolved.descriptor.clone(),
        engine_version: blob.clone(),
        git_version: blob.clone(),
        manifest_digest: manifest.oid.clone(),
    };
    let review = ids.review(&context).unwrap();
    let identifiers: BTreeSet<String> = [
        resolved.digest.id(),
        ids.content(&[content_entry("content/a.md", EntryMode::Regular, &blob)])
            .unwrap()
            .id(),
        manifest.id(),
        review.id(),
        ids.change(&review, &group, 0).unwrap().id(),
        ids.plan(&PlanIdentity {
            operation: blob.clone(),
            request_id: blob.clone(),
            review_id: review.id(),
            branch_ref: blob.clone(),
            scope_id: blob.clone(),
            baseline_oid: blob.clone(),
            candidate_oid: blob.clone(),
            restore: vec![blob.clone()],
            decision_mode: domain::DecisionMode::KeepRemaining,
            message: blob.clone(),
        })
        .unwrap()
        .id(),
    ]
    .into_iter()
    .collect();
    assert_eq!(identifiers.len(), 6, "every kind gives its own identity");
}

#[test]
fn identities_work_in_sha1_and_sha256_repositories() {
    let entries = vec![
        content_entry("a.md", EntryMode::Regular, GOLDEN_ALPHA),
        content_entry("b.md", EntryMode::Executable, GOLDEN_BETA),
    ];
    let cases = [
        (TestRepo::init(), "sha1", 40),
        (
            TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256)),
            "sha256",
            64,
        ),
    ];
    let mut identifiers = Vec::new();
    for (repo, format, width) in cases {
        repo.write_file("content/a.md", b"alpha\n");
        repo.commit_all("seed");
        let adapter = adapter_for(&repo);
        let ids = Identities::new(&adapter).expect("the identities load");
        let resolved = resolve(&repo, &["content/"], &[".md"]);

        // The repository object format qualifies every identifier, and the
        // same canonical bytes stay stable inside one format.
        let content = ids.content(&entries).expect("the content digest computes");
        assert_eq!(content.object_format.as_str(), format);
        assert_eq!(content.oid.len(), width);
        assert!(content.oid.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(content.id(), format!("{}:{}", format, content.oid));
        assert_eq!(ids.content(&entries).unwrap(), content);
        identifiers.push(content.id());

        // The resolver's scope digest and the identity path agree.
        assert_eq!(ids.scope(&resolved.descriptor).unwrap(), resolved.digest);
        assert_eq!(
            resolved.digest.id(),
            format!("{}:{}", format, resolved.digest.oid)
        );

        // Working and stored content digests work in the format, and agree
        // on a clean tree.
        let working = working_content_digest(&adapter, &resolved).expect("working content");
        let stored =
            scoped_content_digest(&adapter, &head_oid(&repo), &resolved).expect("stored content");
        assert_eq!(working, stored);
        assert_eq!(working.object_format.as_str(), format);
    }
    // The same canonical bytes give different identifiers across formats.
    assert_ne!(identifiers[0], identifiers[1]);
}

#[test]
fn unsupported_stored_entries_refuse_instead_of_narrowing() {
    let repo = TestRepo::init();
    repo.write_file("content/real.md", b"x\n");
    repo.commit_all("seed");
    let adapter = adapter_for(&repo);
    let resolved = resolve(&repo, &["content/"], &[".md"]);

    // A committed symbolic link at a scoped path refuses the stored digest.
    symlink("real.md", repo.file_path("content/link.md")).expect("the link is created");
    repo.git_ok(["add", "--", "content/link.md"]);
    repo.commit_all("add link");
    let head = head_oid(&repo);
    let error = scoped_content_digest(&adapter, &head, &resolved)
        .expect_err("the digest refuses a symlink");
    assert!(matches!(error, IdentityError::UnsupportedEntry { .. }));
    assert_eq!(error.public_code(), "UNSUPPORTED_REPOSITORY");
    assert!(error.to_string().contains("symbolic link"));

    // A stored path that is not valid UTF-8 refuses as well.
    let bad = OsString::from_vec(b"content/ba\xffd.md".to_vec());
    // APFS rejects non-UTF-8 filenames, while Git trees can still contain
    // them. Seed the index directly so this exercises stored-entry policy
    // on both macOS and Linux without relying on filesystem acceptance.
    let blob = String::from_utf8(repo.git_ok(["rev-parse", "HEAD:content/real.md"])).unwrap();
    repo.git_ok(vec![
        OsString::from("update-index"),
        "--add".into(),
        "--cacheinfo".into(),
        "100644".into(),
        blob.trim().into(),
        bad.clone(),
    ]);
    repo.git_ok(["commit", "-m", "odd bytes"]);
    let head = head_oid(&repo);
    let error = scoped_content_digest(&adapter, &head, &resolved)
        .expect_err("the digest refuses a non-UTF-8 path");
    assert!(matches!(error, IdentityError::UnsupportedEntry { .. }));
    assert_eq!(error.public_code(), "UNSUPPORTED_REPOSITORY");
    assert!(error.to_string().contains("UTF-8"));
}
