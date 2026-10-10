# Margin

Save and review Markdown edits through ordinary Git history.

![Margin terminal session showing pending changes, selective review, reviewed status, and Git history.](docs/assets/margin-terminal.png)

Margin is a command-line tool for writing projects. Capture a draft,
inspect its changes, then keep or restore selected passages.

## Quick start

You need Git and Rust with Cargo. The repository pins its Rust toolchain
in `rust-toolchain.toml`.

```bash
git clone https://github.com/nibzard/margin.git
cd margin
cargo install --path . --locked
margin --help
```

Cargo installs `margin` in its binary directory. Add that directory to
your `PATH` if your shell cannot find the command.

Try the included documents in a separate repository:

```bash
MARGIN_DEMO=$(mktemp -d "${TMPDIR:-/tmp}/margin-demo.XXXXXX")
mkdir -p "$MARGIN_DEMO/content"
cp examples/before/*.md "$MARGIN_DEMO/content/"
cd "$MARGIN_DEMO"
git init -b writing
git config --local user.name "Demo Writer"
git config --local user.email "demo@example.test"
margin init --scope content/
margin status
```

Edit a file in `content/`, then save and inspect the draft:

```bash
margin checkpoint --message "Revise the garden documents"
margin diff
```

Follow the [example walkthrough](examples/README.md) to try selective review,
request retries, and historical restoration. It includes original and edited
documents plus commands that build review plans from your current history.

## How it works

Margin tracks preservation and review separately. A checkpoint saves your
draft. A review records your decisions about that draft.

| Command | Purpose |
| --- | --- |
| `margin init --scope content/` | Adopt a writing scope and record its starting point |
| `margin status` | See whether scoped content is captured and reviewed |
| `margin checkpoint` | Save scoped edits as a draft commit |
| `margin diff` | Inspect changes since the last valid review baseline |
| `margin review --input review.json --dry-run` | Preview the decisions in a review plan |
| `margin review --input review.json` | Keep or restore changes and record the review |
| `margin log` | Browse the writing history |
| `margin show --path content/article.md --revision HEAD` | Read a saved document |
| `margin restore --input restore.json` | Restore one historical document in a new commit |
| `margin doctor` | Inspect recovery state |

Review plans use identifiers from `margin diff --json`. A completed request
has a receipt in Git history. Repeating the same request returns that receipt.
Historical restoration creates a new commit and leaves the restored content
pending review.

Use `--json` for structured command results. Use `--help` on any command
for its options. Files outside the writing scope stay outside Margin's
capture and review operations.

## Development status

The implementation is in progress. Core command-line workflows are available
for testing. The interactive terminal interface, editor and agent integrations,
and parts of explicit recovery remain unfinished.

Known defects and remaining work are recorded in [to-do.json](to-do.json).
Use the disposable example repository when evaluating the current behavior.

## Build and test

Run these commands from the Margin repository:

```bash
cargo build --locked
cargo test --all-targets --locked
```

The tests use temporary Git repositories to check file content, history,
scope boundaries, receipts, and failure behavior.

For more detail, see:

- [Examples and demonstration](examples/README.md)
- [Behavior and design specification](SPEC.md)
- [Review plan schema](schemas/review-plan-v1.json)
- [Formal model](formal/MarginReview.tla)
- [Formal mutation checks](formal/mutations/README.md)
- [Implementation backlog](to-do.json)

## License

Licensed under the [Apache License 2.0](LICENSE).
