//! Independent correctness oracles for every timed run (SPEC 17.7.6).
//!
//! An oracle observes the fixture through stock Git and raw bytes only,
//! never through Margin's own parser or a second Margin command. A fast
//! command that fails its oracle fails the benchmark: the failures are
//! collected and recorded with the sample.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

/// One stock-Git view of the fixture repository.
pub struct GitOracle {
    repo: PathBuf,
    vars: BTreeMap<OsString, OsString>,
}

impl GitOracle {
    /// An oracle over `repo`, using the same scrubbed environment as the
    /// timed children.
    pub fn new(repo: &Path, vars: BTreeMap<OsString, OsString>) -> Self {
        GitOracle {
            repo: repo.to_path_buf(),
            vars,
        }
    }

    /// One prepared Git command with optional extra environment.
    fn command(&self, args: &[&str], extra: &BTreeMap<String, String>) -> Command {
        let mut command = Command::new("git");
        command.current_dir(&self.repo).args(args);
        for (key, _) in std::env::vars_os() {
            let scrubs = key.as_encoded_bytes().starts_with(b"GIT_") || key == "EMAIL";
            if scrubs {
                command.env_remove(&key);
            }
        }
        command.envs(&self.vars);
        for (key, value) in extra {
            command.env(key, value);
        }
        command.stdin(Stdio::null());
        command
    }

    /// Runs Git and returns the raw output.
    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args, &BTreeMap::new())
            .output()
            .unwrap_or_else(|error| panic!("cannot run git {}: {error}", args.join(" ")))
    }

    /// Runs Git, asserts success, and returns decoded stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Runs Git with one extra environment entry, asserting success.
    fn ok_with(&self, args: &[&str], extra: &BTreeMap<String, String>) -> String {
        let output = self
            .command(args, extra)
            .output()
            .unwrap_or_else(|error| panic!("cannot run git {}: {error}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The `HEAD` commit ID.
    pub fn head(&self) -> String {
        self.ok(&["rev-parse", "HEAD"])
    }

    /// The tree of `revision`.
    pub fn tree_of(&self, revision: &str) -> String {
        self.ok(&["rev-parse", &format!("{revision}^{{tree}}")])
    }

    /// The first-parent ancestry of `HEAD`, newest first.
    pub fn ancestry(&self, limit: usize) -> Vec<String> {
        self.ok(&[
            "rev-list",
            "--first-parent",
            "-n",
            &limit.to_string(),
            "HEAD",
        ])
        .lines()
        .map(str::to_string)
        .collect()
    }

    /// The raw commit object bytes of `revision`.
    pub fn commit_raw(&self, revision: &str) -> String {
        String::from_utf8_lossy(&self.run(&["cat-file", "commit", revision]).stdout).into_owned()
    }

    /// The working-tree changes `git status --porcelain` reports.
    pub fn status_porcelain(&self) -> String {
        self.ok(&["status", "--porcelain"])
    }

    /// The paths Git itself says differ between two revisions.
    pub fn changed_paths(&self, baseline: &str, candidate: &str) -> Vec<String> {
        self.ok(&["diff", "--name-only", baseline, candidate])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The index-tree entry of `path` at `revision`: mode and blob ID.
    pub fn tree_entry(&self, revision: &str, path: &str) -> Option<(String, String)> {
        let line = self.ok(&["ls-tree", revision, path]);
        let mut fields = line.split_whitespace();
        let mode = fields.next()?.to_string();
        fields.next()?;
        let oid = fields.next()?.to_string();
        Some((mode, oid))
    }

    /// The bytes of one blob.
    pub fn blob_bytes(&self, oid: &str) -> Vec<u8> {
        self.run(&["cat-file", "blob", oid]).stdout
    }

    /// The object ID Git assigns to `bytes`, storing the object so later
    /// index plumbing can build a tree from it.
    pub fn hash_object(&self, bytes: &[u8]) -> String {
        let mut command = self.command(&["hash-object", "-w", "--stdin"], &BTreeMap::new());
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().expect("cannot spawn git hash-object");
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("hash-object stdin")
            .write_all(bytes)
            .expect("cannot feed hash-object");
        let output = child.wait_with_output().expect("hash-object finishes");
        assert!(output.status.success(), "git hash-object failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The tree Git's own index plumbing produces from `base_tree` with
    /// `replacements` swapped in: the expected result tree of a mutation,
    /// derived without Margin.
    pub fn derived_tree(
        &self,
        base_tree: &str,
        replacements: &[(String, String, String)],
        scratch_index: &Path,
    ) -> String {
        let _ = fs::remove_file(scratch_index);
        let mut extra = BTreeMap::new();
        extra.insert(
            "GIT_INDEX_FILE".to_string(),
            scratch_index.display().to_string(),
        );
        self.ok_with(&["read-tree", base_tree], &extra);
        for (mode, oid, path) in replacements {
            self.ok_with(
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{oid},{path}"),
                ],
                &extra,
            );
        }
        self.ok_with(&["write-tree"], &extra)
    }

    /// The working-tree bytes of one repository-relative path.
    pub fn working_bytes(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.repo.join(path)).ok()
    }
}

/// Parses the CLI JSON envelope of a timed run, or reports why it cannot
/// be trusted.
pub fn envelope(stdout: &[u8], stderr: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(stdout)
        .map_err(|error| format!("the response is not UTF-8: {error}"))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("the response is not one JSON envelope: {error}"))?;
    let map = value
        .as_object()
        .ok_or_else(|| "the response is not a JSON object".to_string())?;
    if !map.contains_key("ok") {
        return Err("the envelope carries no `ok` field".to_string());
    }
    if map["ok"] != Value::Bool(true) {
        let problem = serde_json::to_string(&map["error"]).unwrap_or_default();
        return Err(format!("the command reported failure: {problem}"));
    }
    if !stderr.is_empty() {
        return Err(format!(
            "the command wrote {} stderr bytes: {}",
            stderr.len(),
            String::from_utf8_lossy(stderr)
        ));
    }
    map.get("data")
        .cloned()
        .ok_or_else(|| "the envelope carries no data".to_string())
}

/// The facts the corpus manifest records that the oracles compare
/// against, pre-extracted in the fixture.
#[derive(Clone, Debug)]
pub struct ManifestFacts {
    /// The candidate commit C.
    pub candidate: String,
    /// The tree of C.
    pub candidate_tree: String,
    /// The baseline review commit B.
    pub baseline: String,
    /// The tree of B.
    pub baseline_tree: String,
    /// Changed documents between B and C.
    pub changed_documents: usize,
    /// Separated change groups between B and C.
    pub changed_groups: u64,
    /// The selected benchmark document path.
    pub selected_path: String,
    /// Per changed document: path and baseline-side blob.
    pub changed_before_blobs: Vec<(String, String)>,
    /// Per changed document: path and candidate-side blob.
    pub changed_after_blobs: Vec<(String, String)>,
}

/// Reads the manifest facts out of a parsed corpus manifest.
pub fn manifest_facts(manifest: &Value) -> ManifestFacts {
    let changed: Vec<(String, String, String)> = manifest["content"]["changed"]
        .as_array()
        .expect("the manifest lists changed documents")
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().expect("a path").to_string(),
                entry["before_blob"]
                    .as_str()
                    .expect("a baseline blob")
                    .to_string(),
                entry["after_blob"]
                    .as_str()
                    .expect("a candidate blob")
                    .to_string(),
            )
        })
        .collect();
    ManifestFacts {
        candidate: manifest["graph"]["head"]
            .as_str()
            .expect("a candidate")
            .to_string(),
        candidate_tree: manifest["graph"]["head_tree"]
            .as_str()
            .expect("a candidate tree")
            .to_string(),
        baseline: manifest["graph"]["baseline_commit"]
            .as_str()
            .expect("a baseline")
            .to_string(),
        baseline_tree: manifest["graph"]["baseline_tree"]
            .as_str()
            .expect("a baseline tree")
            .to_string(),
        changed_documents: manifest["layout"]["changed_documents"]
            .as_u64()
            .expect("a count") as usize,
        changed_groups: manifest["expected"]["changed_groups"]
            .as_u64()
            .expect("a count"),
        selected_path: manifest["layout"]["selected"]["path"]
            .as_str()
            .expect("a selected path")
            .to_string(),
        changed_before_blobs: changed
            .iter()
            .map(|(path, before, _)| (path.clone(), before.clone()))
            .collect(),
        changed_after_blobs: changed
            .iter()
            .map(|(path, _, after)| (path.clone(), after.clone()))
            .collect(),
    }
}

/// Collects the failures of `check` into `failures`, formatted with the
/// metric context.
pub fn check(failures: &mut Vec<String>, context: &str, check: Result<(), String>) {
    if let Err(problem) = check {
        failures.push(format!("{context}: {problem}"));
    }
}

/// P-HELP: the root help answered with one usable envelope.
pub fn verify_help(stdout: &[u8], stderr: &[u8]) -> Result<(), String> {
    let data = envelope(stdout, stderr)?;
    let usage = data["usage"]
        .as_str()
        .ok_or_else(|| "the help carries no usage".to_string())?;
    if usage.is_empty() {
        return Err("the help usage is empty".to_string());
    }
    Ok(())
}

/// P-STATUS: the report agrees with stock Git and the manifest.
pub fn verify_status(
    oracle: &GitOracle,
    facts: &ManifestFacts,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), String> {
    let data = envelope(stdout, stderr)?;
    let head = data["head"].as_str().ok_or("no head")?;
    let baseline = data["baseline"]["commit"]
        .as_str()
        .ok_or("no baseline commit")?;
    let pending = data["counts"]["pending_files"]
        .as_u64()
        .ok_or("no pending count")?;
    let git_head = oracle.head();
    if head != git_head {
        return Err(format!("reported head {head} but Git says {git_head}"));
    }
    if baseline != facts.baseline {
        return Err(format!(
            "reported baseline {baseline} but the manifest pins {}",
            facts.baseline
        ));
    }
    let git_changed = oracle.changed_paths(&facts.baseline, &facts.candidate);
    if git_changed.len() as u64 != pending {
        return Err(format!(
            "reported {pending} pending files but Git lists {} between B and C",
            git_changed.len()
        ));
    }
    if git_changed.len() != facts.changed_documents {
        return Err(format!(
            "Git lists {} changed documents but the manifest records {}",
            git_changed.len(),
            facts.changed_documents
        ));
    }
    Ok(())
}

/// P-DIFF: the selected-document review data agrees with stock Git and
/// the manifest's group totals.
pub fn verify_diff(
    oracle: &GitOracle,
    facts: &ManifestFacts,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), String> {
    let data = envelope(stdout, stderr)?;
    let candidate = data["candidate_oid"].as_str().ok_or("no candidate")?;
    let baseline = data["baseline_oid"].as_str().ok_or("no baseline")?;
    let groups = data["scope_totals"]["groups"]
        .as_u64()
        .ok_or("no group total")?;
    if candidate != oracle.head() {
        return Err(format!(
            "reported candidate {candidate} but Git says HEAD is {}",
            oracle.head()
        ));
    }
    if baseline != facts.baseline {
        return Err(format!(
            "reported baseline {baseline} but the manifest pins {}",
            facts.baseline
        ));
    }
    if groups != facts.changed_groups {
        return Err(format!(
            "reported {groups} groups but the manifest records {}",
            facts.changed_groups
        ));
    }
    let shown: Vec<&str> = data["changes"]
        .as_array()
        .ok_or("no change array")?
        .iter()
        .map(|change| change["path"].as_str().ok_or("a change without a path"))
        .collect::<Result<_, _>>()?;
    // Diff rows describe passage groups, so one selected document can
    // appear in several rows. Every row must still name that document.
    if shown.is_empty() || shown.iter().any(|path| *path != facts.selected_path) {
        return Err(format!(
            "the selected-document filter showed {shown:?} instead of [{}]",
            facts.selected_path
        ));
    }
    let git_changed = oracle.changed_paths(&facts.baseline, &facts.candidate);
    if !git_changed.contains(&facts.selected_path) {
        return Err("Git does not see the selected document as changed".to_string());
    }
    Ok(())
}

/// P-HISTORY: the timeline starts at Git's `HEAD` and stays inside
/// Git's first-parent ancestry within the bound.
pub fn verify_history(
    oracle: &GitOracle,
    stdout: &[u8],
    stderr: &[u8],
    limit: u64,
) -> Result<(), String> {
    let data = envelope(stdout, stderr)?;
    let head = data["head"].as_str().ok_or("no head")?;
    let ancestry = oracle.ancestry(10_000);
    if head != ancestry[0] {
        return Err(format!(
            "the timeline starts at {head} but Git says HEAD is {}",
            ancestry[0]
        ));
    }
    let entries: Vec<&str> = data["entries"]
        .as_array()
        .ok_or("no entries")?
        .iter()
        .map(|entry| entry["commit"].as_str().ok_or("an entry without a commit"))
        .collect::<Result<_, _>>()?;
    if entries.len() as u64 > limit {
        return Err(format!(
            "{} entries exceeded the bound of {limit}",
            entries.len()
        ));
    }
    if entries.is_empty() {
        return Err("the timeline is empty over a populated history".to_string());
    }
    for commit in &entries {
        if !ancestry.contains(&commit.to_string()) {
            return Err(format!(
                "entry {commit} is outside Git's first-parent ancestry"
            ));
        }
    }
    Ok(())
}

/// P-CHECKPOINT: exactly one draft marker moved `HEAD`, with the dirty
/// bytes committed verbatim and nothing else disturbed.
pub fn verify_checkpoint(
    oracle: &GitOracle,
    facts: &ManifestFacts,
    expected_tree: &str,
    dirty: &[(String, Vec<u8>)],
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), String> {
    envelope(stdout, stderr)?;
    let head = oracle.head();
    if head == facts.candidate {
        return Err("HEAD did not move: the checkpoint was a no-op".to_string());
    }
    let parent = oracle.ok(&["rev-parse", "HEAD^"]);
    if parent != facts.candidate {
        return Err(format!(
            "the new commit's parent is {parent}, not the candidate {}",
            facts.candidate
        ));
    }
    let tree = oracle.tree_of("HEAD");
    if tree != expected_tree {
        return Err(format!(
            "the committed tree is {tree} but the fixture's dirty bytes derive {expected_tree}"
        ));
    }
    let raw = oracle.commit_raw("HEAD");
    if !raw.contains("\nMargin-Kind: draft\n") {
        return Err("the new commit carries no draft marker trailer".to_string());
    }
    let dirty_status = oracle.status_porcelain();
    if !dirty_status.is_empty() {
        return Err(format!(
            "the working tree is dirty after the checkpoint: {dirty_status}"
        ));
    }
    for (path, bytes) in dirty {
        let working = oracle
            .working_bytes(path)
            .ok_or_else(|| format!("the dirty document {path} vanished"))?;
        if &working != bytes {
            return Err(format!(
                "the working bytes of {path} changed during the checkpoint"
            ));
        }
    }
    Ok(())
}

/// P-APPLY: exactly one review marker moved `HEAD` onto the fixture's
/// independently derived selective result tree, restoring exactly the
/// planned documents in the working tree.
// The parameter list mirrors the oracle facts of the SPEC 17.7.6 checks;
// grouping them into a struct would only rename the same eight values.
#[allow(clippy::too_many_arguments)]
pub fn verify_review(
    oracle: &GitOracle,
    facts: &ManifestFacts,
    expected_tree: &str,
    restored: &[(String, String)],
    kept: &[(String, String)],
    request_id: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), String> {
    envelope(stdout, stderr)?;
    let head = oracle.head();
    if head == facts.candidate {
        return Err("HEAD did not move: the review was a no-op".to_string());
    }
    let parent = oracle.ok(&["rev-parse", "HEAD^"]);
    if parent != facts.candidate {
        return Err(format!(
            "the review commit's parent is {parent}, not the candidate {}",
            facts.candidate
        ));
    }
    let tree = oracle.tree_of("HEAD");
    if tree != expected_tree {
        return Err(format!(
            "the review tree is {tree} but independent index plumbing derives {expected_tree}"
        ));
    }
    let raw = oracle.commit_raw("HEAD");
    for trailer in [
        "\nMargin-Kind: review\n",
        &format!("\nMargin-Result-Tree: {expected_tree}\n"),
        &format!("\nMargin-Request: {request_id}\n"),
    ] {
        if !raw.contains(trailer) {
            return Err(format!(
                "the review commit lacks the trailer {:?}",
                trailer.trim()
            ));
        }
    }
    let dirty_status = oracle.status_porcelain();
    if !dirty_status.is_empty() {
        return Err(format!(
            "the working tree is dirty after the review: {dirty_status}"
        ));
    }
    for (path, blob) in restored {
        let working = oracle
            .working_bytes(path)
            .ok_or_else(|| format!("the restored document {path} vanished"))?;
        let expected = oracle.blob_bytes(blob);
        if working != expected {
            return Err(format!("{path} was not restored to its baseline bytes"));
        }
    }
    for (path, blob) in kept {
        let working = oracle
            .working_bytes(path)
            .ok_or_else(|| format!("the kept document {path} vanished"))?;
        let expected = oracle.blob_bytes(blob);
        if working != expected {
            return Err(format!("{path} did not keep its candidate bytes"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_envelope_is_rejected_for_the_right_reason() {
        let failures = envelope(b"not json", b"").unwrap_err();
        assert!(failures.contains("not one JSON envelope"));
        let failures = envelope(br#"{"ok":false,"error":{"code":"X"}}"#, b"").unwrap_err();
        assert!(failures.contains("reported failure"));
        let failures = envelope(br#"{"ok":true,"data":{}}"#, b"a warning").unwrap_err();
        assert!(failures.contains("stderr"));
        assert!(envelope(br#"{"ok":true,"data":{"usage":"margin"}}"#, b"").is_ok());
    }
}
