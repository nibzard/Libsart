# Source editing validation

Date: 2026-10-15. These observations describe local implementation validation.
The implementation is not tagged or published by this task.

## Autosave revision

The agreed workflow adds default autosave after 750 milliseconds, retained
buffers, background saving, and requests from the latest source. Results can
apply while the editor stays open. Conservative span comparison preserves a
disjoint writer edit. Explicit conflict choices and guarded agent undo retain
writer control. Request and result boundaries preserve history automatically.

The focused checks pass for autosave, desk navigation, source editing, pointer
access, hover, and refresh: 89 tests across six targets. A subsequent focused
run passes 72 tests across four targets. It covers the final recovery guard,
requests, adapter guidance, and access.
The terminal source test now waits for saved bytes without sending `Ctrl+S`.
Recovered text pauses autosave until the writer compares its disk base.

The final lints and Rust 1.89.0 gates pass. The release budgets gate measures
4,548,472 stripped bytes and a local help 95th percentile of 1.7 ms. Both pass
the existing limits. The full test gate passes 962 tests across 56 targets
in 389 seconds. The 72-test focused run also covers the final recovery guard
and text changes made while that full gate ran.
The formal model is unchanged; the earlier formal checks below do not model
these new interactions.

```bash
env -u NO_COLOR cargo test --locked --test autosave --test source_editing \
  --test tui_access --test tui_hover --test tui_refresh --test desk_open
env -u NO_COLOR cargo test --locked --test source_editing --test autosave \
  --test notes_channel --test tui_access
```

## Initial implementation checks

The full pull-request test gate passed 951 tests across 55 targets. A later
focused run passed 288 tests across seven targets after the final recovery,
request, and adapter changes. The focused run includes 18 source editing tests. The final key-binding check
also passed 242 tests across four targets, including a real-terminal `F2`
selection request.

The new coverage verifies:

- UTF-8 grapheme movement, CRLF, empty files, and final newline fidelity.
- Literal paste, undo, redo, resize, save, and terminal mode restoration.
- Save conflicts, compared disk versions, missing files, and recovery records.
- Scope, branch, symlink, permission, and source-size refusals.
- Unchanged Git index, out-of-scope content, and approval state after saves.
- Concurrent claims, repeated submissions, stale results, and interrupted intent.
- Generic agent commands and bounded Claude prompt context.
- Passage selection, deferred application, checkpointing, and explanation notes.
- Two desk viewers with independent sockets and an open editor at the origin.
- Versioned socket status, notification, and unsupported-version refusal.

Commands:

```bash
env -u NO_COLOR ci/run-tier.sh tests pr
env -u NO_COLOR cargo test --locked --lib --test source_editing \
  --test cli_contract --test tui_access --test desk_open \
  --test notes_channel --test tui_lifecycle
```

`NO_COLOR` must be absent for terminal assertions that inspect decorated output.
The full test gate also requires tmux, as defined in the existing tier script.

## Build checks

The lints gate and Rust 1.89.0 compatibility gate pass. The release budgets
gate measured a stripped executable of 4,464,736 bytes against the 16 MiB limit.
Its local help measurement was 1.6 ms at the 95th percentile against the gate's
750 ms shared-runner ceiling.

The corresponding executable before stripping measured 5,572,944 bytes.
The earlier release executable in this workspace measured 5,231,920 bytes
before stripping. These totals include all feature changes. They do not
isolate the cost of Unicode segmentation. The resolved dependency graph adds
no package or version; `unicode-segmentation` was already present through Ratatui.

```bash
env -u NO_COLOR ci/run-tier.sh lints pr
env -u NO_COLOR ci/run-tier.sh msrv pr
env -u NO_COLOR ci/run-tier.sh budgets pr
```

## Rendering diagnostic

A release-build test opens complete 1 MiB source, moves to the end of its
single ASCII line, and renders a 110-column frame. The buffer retains every
byte, including the trailing `END` marker.

One early local observation measured 154 ms to load and 72 ms for End plus
rendering. After limiting allocations to visible cells and using byte spans
for plain ASCII, a later observation measured 58 ms to load and 4 ms for
End plus rendering. Both observations used the same functional test. Other
verification work ran concurrently, so these are diagnostic observations.

```bash
env -u NO_COLOR cargo test --release --locked --test source_editing \
  a_complete_long_source_line -- --nocapture
```

This check does not replace the registered P-KEY campaign. It measures a buffer
operation and rendering, rather than 10,000 events injected through a terminal.
It does not establish a physical terminal or macOS performance claim.

## Formal checks

The existing pull-request formal gate passes. Its safety, liveness, and
cooperative profiles pass. The uncooperative profile finds its expected
`NoUnseenOverwrite` counterexample. These profiles validate the existing
review model; they do not model the new editor or request queue.
The new queue contracts are covered by real-Git and process tests above.

```bash
env -u NO_COLOR ci/run-tier.sh formal pr
```

Gate logs and environment records are retained under `ci/artifacts/` in this
workspace. The new source and request contracts are documented in
[SPEC 12.8](../SPEC.md#128-built-in-source-editing-and-agent-requests),
[the user guide](source-editing.md), and
[the decision record](adr-source-editor.md).
