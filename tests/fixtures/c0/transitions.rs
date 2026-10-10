//! File presence and mode transitions.
//!
//! Hand-reviewed fixture pairs of the C0 correctness corpus. The hunk
//! layout and the per-group restoration bytes below were produced once
//! with stock Git under the fixed contract of SPEC 10.1 and then frozen;
//! the `c0_corpus` test re-derives them on every run.

use super::{Expectation, ExpectedChanges, FileChange, Fixture, HunkRange, Side};

pub const FIXTURES: &[Fixture] = &[
    Fixture {
        id: "file-added",
        title: "A new document appears",
        classes: &["file-presence"],
        path: "added.md",
        before: Side::ABSENT,
        after: Side::present_text(concat!("a new document\n", "second line\n",), 0o644),
        changes: ExpectedChanges {
            change: FileChange::Added,
            binary: false,
            hunks: &[HunkRange::new(0, 0, 1, 2)],
        },
        expectation: Expectation::whole_file(
            "file creation: an added file is a whole-file decision",
        ),
        reader: concat!(
            "An explicit presence transition with a full-file preview; restoring ",
            "removes the file exactly.",
        ),
    },
    Fixture {
        id: "file-deleted",
        title: "An existing document disappears",
        classes: &["file-presence"],
        path: "removed.md",
        before: Side::present_text("an old document\n", 0o644),
        after: Side::ABSENT,
        changes: ExpectedChanges {
            change: FileChange::Deleted,
            binary: false,
            hunks: &[HunkRange::new(1, 1, 0, 0)],
        },
        expectation: Expectation::whole_file(
            "file deletion: a removed file is a whole-file decision",
        ),
        reader: concat!(
            "The deletion is a presence transition; restoring brings the exact ",
            "bytes back.",
        ),
    },
    Fixture {
        id: "file-mode-change",
        title: "Only the executable bit changes",
        classes: &["file-mode"],
        path: "tool.md",
        before: Side::present_text(concat!("#!/bin/sh\n", "echo tool\n",), 0o644),
        after: Side::present_text(concat!("#!/bin/sh\n", "echo tool\n",), 0o755),
        changes: ExpectedChanges {
            change: FileChange::ModeOnly,
            binary: false,
            hunks: &[],
        },
        expectation: Expectation::whole_file(concat!(
            "unsupported file-mode change: the whole file is the decision while ",
            "the bytes stay identical",
        )),
        reader: concat!(
            "Only the executable bit changes; the mode transition is reviewed as ",
            "the whole file with identical bytes.",
        ),
    },
];
