//! The benchmark fixture and its mutation reset (SPEC 17.3, 17.7.2).
//!
//! A fixture is one generated corpus: the repository the timed process
//! runs in, its manifest, and a private home with the fixture identity
//! the mutation commands commit under. Fixture work — generation,
//! reset, plan construction, oracle inspection — happens strictly
//! outside the timed window, and nothing the product does is moved into
//! that preparation.
//!
//! Every mutation iteration begins with the intended dirty or reviewable
//! state: the reset restores `HEAD`, the index, and the working tree to
//! the candidate C, rewrites the exact dirty bytes P-CHECKPOINT
//! commits, and asserts the pre-state before the clock starts, so a run
//! can never degrade into a no-op or a receipt lookup.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::envrec::child_environment;
use super::oracle::{GitOracle, ManifestFacts, manifest_facts};

/// The paragraph P-CHECKPOINT appends to the dirty documents. It is a
/// fixed constant, so every iteration commits byte-identical content
/// and the expected tree never changes.
const DIRTY_PARAGRAPH: &[u8] =
    b"\n\nThe bench tally records 3 of 4 marks, and the quick guide keeps its words.\n";

/// How many documents P-CHECKPOINT dirties: the selected document and
/// one ordinary changed document.
const DIRTY_DOCUMENTS: usize = 2;

/// The fixture identity the mutation commands commit under; the same
/// reserved domain the corpus generator uses.
const FIXTURE_USER_NAME: &str = "Margin Bench Fixture";
const FIXTURE_USER_EMAIL: &str = "margin-bench@example.invalid";

/// What one reset prepared for the timed run that follows.
pub enum Prepared {
    /// The repository alone is the workload.
    Repository,
    /// A review run reads this plan and commits under this request
    /// identifier.
    Review {
        /// The plan document path.
        plan: PathBuf,
        /// The request identifier the plan pins.
        request_id: String,
    },
}

/// One prepared benchmark fixture.
pub struct Fixture {
    /// The corpus root: `repo/` and `manifest.json` live here.
    pub root: PathBuf,
    /// The repository the timed process runs in.
    pub repo: PathBuf,
    /// The parsed corpus manifest.
    pub manifest: Value,
    /// SHA-256 of the manifest bytes.
    pub manifest_sha256: String,
    /// The generator revision the manifest records.
    pub generator_revision: String,
    /// The seed the manifest records.
    pub seed: u64,
    /// The private home the timed processes and the oracle Git share.
    pub home: PathBuf,
    /// The manifest facts the oracles compare against.
    pub facts: ManifestFacts,
    /// The stock-Git view the fixture owns.
    pub oracle: GitOracle,
    /// Documents P-CHECKPOINT dirties, with their exact dirty bytes.
    pub checkpoint_dirty: Vec<(String, Vec<u8>)>,
    /// The tree independent index plumbing derives for the checkpoint.
    pub checkpoint_expected_tree: String,
    /// Documents P-APPLY restores, with their baseline blobs.
    pub review_restored: Vec<(String, String)>,
    /// Documents P-APPLY keeps, with their candidate blobs.
    pub review_kept: Vec<(String, String)>,
    /// The tree independent index plumbing derives for the review.
    pub review_expected_tree: String,
    /// The scratch directory for oracle index files and plan inputs.
    pub scratch: PathBuf,
    /// The environment map every child runs under.
    pub vars: BTreeMap<OsString, OsString>,
}

impl Fixture {
    /// Prepares the fixture over one generated corpus root.
    ///
    /// This is untimed preparation: the dirty bytes, the expected trees,
    /// and the review split are derived once, through stock Git, before
    /// any warmup runs.
    pub fn prepare(root: &Path, scratch: &Path) -> Result<Fixture, String> {
        let manifest_path = root.join("manifest.json");
        let manifest_bytes = fs::read(&manifest_path)
            .map_err(|error| format!("{} cannot be read: {error}", manifest_path.display()))?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| format!("{} is not the manifest JSON: {error}", manifest_path.display()))?;
        let repo = root.join("repo");
        if !repo.join(".git").exists() {
            return Err(format!(
                "{} holds no repository; generate the corpus first",
                repo.display()
            ));
        }
        fs::create_dir_all(scratch)
            .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
        let home = scratch.join("bench-home");
        fs::create_dir_all(home.join(".config"))
            .map_err(|error| format!("cannot create the bench home: {error}"))?;
        fs::write(
            home.join(".gitconfig"),
            format!(
                "[user]\n\tname = {FIXTURE_USER_NAME}\n\temail = {FIXTURE_USER_EMAIL}\n\
                 [commit]\n\tgpgsign = false\n[gc]\n\tauto = 0\n"
            ),
        )
        .map_err(|error| format!("cannot write the fixture gitconfig: {error}"))?;

        let facts = manifest_facts(&manifest);
        let vars = child_environment(&home);
        let oracle = GitOracle::new(&repo, vars.clone());
        let fixture = Fixture {
            manifest_sha256: super::hash::sha256_hex(&manifest_bytes),
            generator_revision: manifest["generator_revision"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            seed: manifest["seed"].as_u64().unwrap_or(0),
            root: root.to_path_buf(),
            repo,
            manifest,
            facts,
            home,
            oracle,
            checkpoint_dirty: Vec::new(),
            checkpoint_expected_tree: String::new(),
            review_restored: Vec::new(),
            review_kept: Vec::new(),
            review_expected_tree: String::new(),
            scratch: scratch.to_path_buf(),
            vars,
        };
        // The dirty bytes and expected trees must describe the candidate
        // state, so the fixture returns to C before it derives them; a
        // corpus left mid-mutation by an earlier measurement would
        // otherwise poison every later workload.
        fixture.reset_to_candidate()?;
        fixture.assert_reviewable()?;
        fixture.derive_workloads()
    }

    /// Derives both mutation workloads through stock Git plumbing.
    fn derive_workloads(mut self) -> Result<Self, String> {
        // P-CHECKPOINT dirties the selected document and the next changed
        // document, appending one fixed paragraph.
        let mut dirty_paths: Vec<String> = Vec::new();
        for (path, _) in &self.facts.changed_after_blobs {
            if path == &self.facts.selected_path {
                continue;
            }
            if dirty_paths.len() + 1 < DIRTY_DOCUMENTS {
                dirty_paths.push(path.clone());
            }
        }
        let mut dirty = vec![(self.facts.selected_path.clone(), Vec::new())];
        dirty.extend(dirty_paths.into_iter().map(|path| (path, Vec::new())));
        let mut replacements = Vec::new();
        for (path, bytes) in &mut dirty {
            let clean = self
                .oracle
                .working_bytes(path)
                .ok_or_else(|| format!("the corpus lacks the working document {path}"))?;
            bytes.extend_from_slice(&clean);
            bytes.extend_from_slice(DIRTY_PARAGRAPH);
            let mode = self
                .oracle
                .tree_entry(&self.facts.candidate, path)
                .and_then(|(mode, _)| Some(mode))
                .ok_or_else(|| format!("the candidate tree lacks {path}"))?;
            let blob = self.oracle.hash_object(bytes);
            replacements.push((mode, blob, path.clone()));
        }
        self.checkpoint_dirty = dirty;
        self.checkpoint_expected_tree = self.oracle.derived_tree(
            &self.facts.candidate_tree,
            &replacements,
            &self.scratch.join("checkpoint-index"),
        );

        // P-APPLY restores every changed document except the selected
        // one, whose changes stay kept: a selective review by manifest
        // decision.
        let mut restored = Vec::new();
        let mut kept = Vec::new();
        let mut replacements = Vec::new();
        for (path, before) in &self.facts.changed_before_blobs {
            let (mode, _) = self
                .oracle
                .tree_entry(&self.facts.candidate, path)
                .ok_or_else(|| format!("the candidate tree lacks {path}"))?;
            if path == &self.facts.selected_path {
                kept.push((path.clone(), self.facts.changed_after_blobs
                    .iter()
                    .find(|(candidate_path, _)| candidate_path == path)
                    .map(|(_, blob)| blob.clone())
                    .ok_or_else(|| format!("no candidate blob for {path}"))?));
                continue;
            }
            restored.push((path.clone(), before.clone()));
            replacements.push((mode, before.clone(), path.clone()));
        }
        if restored.is_empty() || kept.is_empty() {
            return Err(
                "the corpus change set cannot express a selective review: every document is \
                 either restored or kept"
                    .to_string(),
            );
        }
        self.review_restored = restored;
        self.review_kept = kept;
        self.review_expected_tree = self.oracle.derived_tree(
            &self.facts.candidate_tree,
            &replacements,
            &self.scratch.join("review-index"),
        );
        Ok(self)
    }

    /// Returns the repository to the candidate C: `HEAD`, the index,
    /// and the working tree, with untracked files gone. Every iteration
    /// and every derivation starts here.
    pub fn reset_to_candidate(&self) -> Result<(), String> {
        let reset = self.oracle.run(&["reset", "--hard", &self.facts.candidate]);
        if !reset.status.success() {
            return Err(format!(
                "git reset --hard failed: {}",
                String::from_utf8_lossy(&reset.stderr)
            ));
        }
        let clean = self.oracle.run(&["clean", "-fd"]);
        if !clean.status.success() {
            return Err(format!(
                "git clean -fd failed: {}",
                String::from_utf8_lossy(&clean.stderr)
            ));
        }
        Ok(())
    }

    /// Asserts the repository sits exactly at the intended reviewable
    /// state: `HEAD` at C, clean index and working tree.
    pub fn assert_reviewable(&self) -> Result<(), String> {
        let head = self.oracle.head();
        if head != self.facts.candidate {
            return Err(format!(
                "HEAD is {head}, not the candidate {}; the reset failed",
                self.facts.candidate
            ));
        }
        let dirty = self.oracle.status_porcelain();
        if !dirty.is_empty() {
            return Err(format!(
                "the reviewable state is dirty before the run: {dirty}"
            ));
        }
        Ok(())
    }

    /// Resets the fixture to the state `metric` needs, outside timing,
    /// and returns what the timed run reads.
    ///
    /// The review plan is rebuilt every iteration with a fresh request
    /// identifier, so one iteration can never satisfy the next one
    /// through a retained receipt.
    pub fn reset_for(&self, metric: &str, iteration: usize) -> Result<Prepared, String> {
        self.reset_to_candidate()?;
        match metric {
            "P-HELP" | "P-STATUS" | "P-DIFF" | "P-HISTORY" => {
                self.assert_reviewable()?;
                Ok(Prepared::Repository)
            }
            "P-CHECKPOINT" => {
                self.assert_reviewable()?;
                for (path, bytes) in &self.checkpoint_dirty {
                    let target = self.repo.join(path);
                    fs::write(&target, bytes)
                        .map_err(|error| format!("cannot dirty {path}: {error}"))?;
                }
                let dirty_state = self.oracle.status_porcelain();
                let expected_dirty = self.checkpoint_dirty.len();
                if dirty_state.lines().count() != expected_dirty {
                    return Err(format!(
                        "the dirty state holds {} changes, expected {expected_dirty}: {dirty_state}",
                        dirty_state.lines().count()
                    ));
                }
                Ok(Prepared::Repository)
            }
            "P-APPLY" => {
                self.assert_reviewable()?;
                let plan = self.build_plan(iteration)?;
                Ok(Prepared::Review {
                    request_id: format!("bench-p-apply-i{iteration:06}"),
                    plan,
                })
            }
            other => Err(format!("the harness does not time {other}")),
        }
    }

    /// Whether this fixture runs at the exact SPEC 17.2 sizes of its
    /// corpus, or below them at the test scale.
    pub fn test_scale(&self) -> bool {
        let documents = self.manifest["layout"]["documents"].as_u64().unwrap_or(0);
        let bytes = self.manifest["layout"]["total_document_bytes"].as_u64().unwrap_or(0);
        let full = match self.manifest["corpus"].as_str().unwrap_or("") {
            "C1" => Some((100u64, 100 * 20 * 1024)),
            "C2" => Some((1_000, 100 * 1024 * 1024)),
            "C3" => Some((10_000, 1024 * 1024 * 1024)),
            _ => None,
        };
        match full {
            Some((full_documents, full_bytes)) => {
                documents != full_documents || bytes != full_bytes
            }
            None => true,
        }
    }

    /// Builds one complete review plan for iteration `iteration`.
    ///
    /// The plan's decision members come from the pinned context derived
    /// in [`Fixture::derive_review_context`], and every iteration uses a
    /// fresh request identifier, so one review can never satisfy the
    /// next through a retained receipt.
    pub fn build_plan(&self, iteration: usize) -> Result<PathBuf, String> {
        let context = self.review_context()?;
        let request_id = format!("bench-p-apply-i{iteration:06}");
        let restore = self.restored_change_ids(&context)?;
        let mut plan = serde_json::Map::new();
        plan.insert("schema_version".to_string(), Value::from(1));
        plan.insert("request_id".to_string(), Value::from(request_id));
        plan.insert("review_id".to_string(), context["review_id"].clone());
        plan.insert("branch_ref".to_string(), context["branch_ref"].clone());
        plan.insert("scope_id".to_string(), context["scope_id"].clone());
        plan.insert("baseline_oid".to_string(), context["baseline_oid"].clone());
        plan.insert("candidate_oid".to_string(), context["candidate_oid"].clone());
        plan.insert(
            "restore".to_string(),
            serde_json::to_value(&restore)
                .map_err(|error| format!("the restore list cannot serialize: {error}"))?,
        );
        plan.insert("keep_remaining".to_string(), Value::from(true));
        plan.insert("message".to_string(), Value::from("bench selective review"));
        let path = self.scratch.join(format!("plan-{iteration:06}.json"));
        let body = serde_json::to_vec_pretty(&Value::Object(plan))
            .map_err(|error| format!("the plan cannot serialize: {error}"))?;
        fs::write(&path, body).map_err(|error| format!("cannot write the plan: {error}"))?;
        Ok(path)
    }

    /// The pinned review context of the fixture, from one untimed
    /// `margin diff` through `executable`.
    fn review_context_via(&self, executable: &Path) -> Result<Value, String> {
        let mut command = std::process::Command::new(executable);
        command
            .args(["diff", "--json"])
            .current_dir(&self.repo)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, _) in std::env::vars_os() {
            let scrubs = key.as_encoded_bytes().starts_with(b"GIT_") || key == "EMAIL";
            if scrubs {
                command.env_remove(&key);
            }
        }
        command.envs(&self.vars);
        let output = command
            .output()
            .map_err(|error| format!("cannot run the untimed diff probe: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "the untimed diff probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let data = super::oracle::envelope(&output.stdout, &output.stderr)?;
        Ok(data)
    }

    /// The cached pinned context, derived once per fixture.
    fn review_context(&self) -> Result<Value, String> {
        if let Ok(cached) = fs::read_to_string(self.scratch.join("review-context.json")) {
            return serde_json::from_str(&cached)
                .map_err(|error| format!("the cached review context is broken: {error}"));
        }
        Err("the review context was not derived; call `derive_review_context` first".to_string())
    }

    /// Derives and caches the pinned review context through one untimed
    /// `margin diff` run. Called during run preparation.
    pub fn derive_review_context(&self, executable: &Path) -> Result<Value, String> {
        let context = self.review_context_via(executable)?;
        let body = serde_json::to_vec_pretty(&context)
            .map_err(|error| format!("the context cannot serialize: {error}"))?;
        fs::write(self.scratch.join("review-context.json"), body)
            .map_err(|error| format!("cannot cache the review context: {error}"))?;
        Ok(context)
    }

    /// The change-group identifiers of the documents the review
    /// restores, from the pinned context.
    fn restored_change_ids(&self, context: &Value) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        for change in context["changes"]
            .as_array()
            .ok_or("the diff context lists no changes")?
        {
            let path = change["path"].as_str().ok_or("a change without a path")?;
            if path == self.facts.selected_path {
                continue;
            }
            ids.push(
                change["change_id"]
                    .as_str()
                    .ok_or("a change without an identifier")?
                    .to_string(),
            );
        }
        if ids.is_empty() {
            return Err("no group to restore: the fixture cannot express a selective review".to_string());
        }
        Ok(ids)
    }
}
