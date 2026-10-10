# Try Margin with a writing project

These fictional garden documents demonstrate draft capture, selective review,
receipt retries, and historical restoration. They contain paragraphs, links,
lists, a table, and task checkboxes.

| Document | Change in `after/` | Demo decision |
| --- | --- | --- |
| `article.md` | Shorter introduction | Keep |
| `field-notes.md` | Unconfirmed schedule and measurement changes | Restore |
| `checklist.md` | New closing task | Keep |

The `before/` files are the starting documents. The `after/` files simulate
an editing pass. Keep both directories as reusable fixtures.

## Walkthrough

Run these steps in one Bash session. Start in the Margin project root.
You need Rust, Git, and `python3`. Each run creates a separate temporary
repository and leaves the example files unchanged.

1. Build Margin and create the demo repository.

   ```bash
   cargo build
   MARGIN_BIN="$PWD/target/debug/margin"
   MARGIN_EXAMPLES="$PWD/examples"
   MARGIN_DEMO=$(mktemp -d "${TMPDIR:-/tmp}/margin-demo.XXXXXX")
   mkdir -p "$MARGIN_DEMO/content"
   cp "$MARGIN_EXAMPLES"/before/*.md "$MARGIN_DEMO/content/"
   cd "$MARGIN_DEMO"
   git init -b writing
   git config --local user.name "Demo Writer"
   git config --local user.email "demo@example.test"
   printf 'Demo repository: %s\n' "$MARGIN_DEMO"
   ```

2. Adopt the writing scope and record its starting point.

   ```bash
   "$MARGIN_BIN" init --scope content/ --json
   git rev-parse HEAD > start-commit.txt
   "$MARGIN_BIN" status
   ```

   The documents are captured. The review state is `starting_point`.
   Only Markdown files inside `content/` belong to the writing scope.

3. Apply the example edits, inspect them, and capture a draft.

   ```bash
   cp "$MARGIN_EXAMPLES"/after/*.md content/
   printf 'An unrelated note outside the scope.\n' > private-note.md
   "$MARGIN_BIN" status
   "$MARGIN_BIN" diff --working
   "$MARGIN_BIN" checkpoint --message "Revise the garden documents" --json
   "$MARGIN_BIN" status
   ```

   Before the checkpoint, status reports uncaptured edits. After it, the
   documents are captured and pending review. `private-note.md` remains
   untracked. A checkpoint saves content; it does not approve that content.

4. Inspect the committed changes and create a review plan.

   ```bash
   "$MARGIN_BIN" diff
   "$MARGIN_BIN" diff --json > diff.json
   python3 - <<'PY'
   import json
   from pathlib import Path

   response = json.loads(Path("diff.json").read_text())
   assert response["ok"], response["error"]
   diff = response["data"]
   assert diff["complete"], "Read all diff pages before building this plan."
   context = (
       "review_id", "branch_ref", "scope_id", "baseline_oid", "candidate_oid"
   )
   plan = {key: diff[key] for key in context}
   restore = [
       change["change_id"] for change in diff["changes"]
       if change["path"] == "content/field-notes.md"
   ]
   assert restore, "The demo expects changes in field-notes.md."
   plan.update(
       schema_version=1,
       request_id="garden-review-1",
       restore=restore,
       keep_remaining=True,
       message="Keep clearer prose and checklist; restore field notes",
   )
   Path("review.json").write_text(json.dumps(plan, indent=2) + "\n")
   PY
   cat review.json
   ```

   The plan restores every change in `field-notes.md`. It explicitly keeps
   the other changes. Plan identifiers come from this repository's diff;
   identifiers copied from another run do not apply.

5. Preview the review, apply it, and retry the same request.

   ```bash
   "$MARGIN_BIN" review --input review.json --dry-run --json
   "$MARGIN_BIN" review --input review.json --json
   "$MARGIN_BIN" review --input review.json --json
   "$MARGIN_BIN" status
   cmp content/article.md "$MARGIN_EXAMPLES/after/article.md"
   cmp content/checklist.md "$MARGIN_EXAMPLES/after/checklist.md"
   cmp content/field-notes.md "$MARGIN_EXAMPLES/before/field-notes.md"
   git status --short
   ```

   The dry run reports the intended result. Applying the plan creates a
   review commit. The retry reports `existing_receipt` and creates no second
   commit. Status reports `reviewed`. The three `cmp` commands produce no
   output when the files match. The private note and plan files stay untracked.

6. Inspect history and restore the original introduction.

   ```bash
   "$MARGIN_BIN" log
   "$MARGIN_BIN" show --path content/article.md --revision "$(cat start-commit.txt)"
   "$MARGIN_BIN" status --json > status.json
   python3 - <<'PY'
   import json
   from pathlib import Path

   response = json.loads(Path("status.json").read_text())
   assert response["ok"], response["error"]
   status = response["data"]
   plan = {
       "schema_version": 1,
       "request_id": "garden-restore-1",
       "branch_ref": status["branch"],
       "scope_id": status["scope"]["digest"],
       "candidate_oid": status["head"],
       "path": "content/article.md",
       "revision": Path("start-commit.txt").read_text().strip(),
       "message": "Restore the original introduction for another pass",
   }
   Path("restore.json").write_text(json.dumps(plan, indent=2) + "\n")
   PY
   "$MARGIN_BIN" restore --input restore.json --dry-run --json
   "$MARGIN_BIN" restore --input restore.json --json
   cmp content/article.md "$MARGIN_EXAMPLES/before/article.md"
   "$MARGIN_BIN" status
   ```

   Restoration creates a new commit and preserves earlier history. The
   previous review baseline stays in place. Status reports `pending` because
   the restored introduction has not been reviewed.

7. Continue with your own edits.

   Edit any file in `content/`, then run `status`, `checkpoint`, and `diff`.
   Create a new plan from the current diff and use a new request identifier.
   Use `restore: []` with `keep_remaining: true` to approve all current changes.

   To repeat this exact demonstration, return to the Margin project root
   and start at step 1. Keep the printed temporary path to revisit a run.

## Current limits

This walkthrough uses the command-line interface. The interactive terminal
interface and explicit recovery commands are still under development.
Use ordinary index entries for this demo; hidden edits under `assume-unchanged`
and `skip-worktree` are tracked in task T061.
