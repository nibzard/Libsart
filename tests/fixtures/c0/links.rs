//! Inline links, images, reference links, and footnotes.
//!
//! Hand-reviewed fixture pairs of the C0 correctness corpus. The hunk
//! layout and the per-group restoration bytes below were produced once
//! with stock Git under the fixed contract of SPEC 10.1 and then frozen;
//! the `c0_corpus` test re-derives them on every run.

use super::{Content, Expectation, ExpectedChanges, FileChange, Fixture, Group, HunkRange, Side};

pub const FIXTURES: &[Fixture] = &[
    Fixture {
        id: "links-target-only",
        title: "A link keeps its label and changes its destination",
        classes: &["links"],
        path: "release notes.md",
        before: Side::present_text("See the [changelog](/old-road.md) for details.\n", 0o644),
        after: Side::present_text("See the [changelog](/new-road.md) for details.\n", 0o644),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 1, 1, 1)],
        },
        expectation: Expectation::groups(&[Group {
            reason: concat!(
                "coupled link group: the label is unchanged while the destination ",
                "changes",
            ),
            hunks: &[1],
            restore: Content::text("See the [changelog](/old-road.md) for details.\n"),
        }]),
        reader: concat!(
            "The label stays 'changelog' and the destination changes; a pretty ",
            "rendering that shows no difference fails.",
        ),
    },
    Fixture {
        id: "links-label-and-target",
        title: "A link changes both label and destination",
        classes: &["links"],
        path: "doc.md",
        before: Side::present_text("Read the [guide](/a.md) soon.\n", 0o644),
        after: Side::present_text("Read the [handbook](/b.md) soon.\n", 0o644),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 1, 1, 1)],
        },
        expectation: Expectation::groups(&[Group {
            reason: "coupled link group: label and destination change together",
            hunks: &[1],
            restore: Content::text("Read the [guide](/a.md) soon.\n"),
        }]),
        reader: concat!(
            "Label and destination change together; each part is shown separately ",
            "so the reader can judge them apart.",
        ),
    },
    Fixture {
        id: "links-two-distant",
        title: "Two distant link destinations change",
        classes: &["links"],
        path: "doc.md",
        before: Side::present_text(
            concat!(
                "First [start](/one.md) here.\n",
                "filler 01\n",
                "filler 02\n",
                "filler 03\n",
                "filler 04\n",
                "filler 05\n",
                "filler 06\n",
                "filler 07\n",
                "filler 08\n",
                "filler 09\n",
                "filler 10\n",
                "Then [middle](/two.md) follows.\n",
            ),
            0o644,
        ),
        after: Side::present_text(
            concat!(
                "First [start](/uno.md) here.\n",
                "filler 01\n",
                "filler 02\n",
                "filler 03\n",
                "filler 04\n",
                "filler 05\n",
                "filler 06\n",
                "filler 07\n",
                "filler 08\n",
                "filler 09\n",
                "filler 10\n",
                "Then [middle](/dos.md) follows.\n",
            ),
            0o644,
        ),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 4, 1, 4), HunkRange::new(9, 4, 9, 4)],
        },
        expectation: Expectation::groups(&[
            Group {
                reason: concat!(
                    "coupled link group: the label is unchanged while the destination ",
                    "changes",
                ),
                hunks: &[1],
                restore: Content::text(concat!(
                    "First [start](/one.md) here.\n",
                    "filler 01\n",
                    "filler 02\n",
                    "filler 03\n",
                    "filler 04\n",
                    "filler 05\n",
                    "filler 06\n",
                    "filler 07\n",
                    "filler 08\n",
                    "filler 09\n",
                    "filler 10\n",
                    "Then [middle](/dos.md) follows.\n",
                )),
            },
            Group {
                reason: concat!(
                    "coupled link group: the label is unchanged while the destination ",
                    "changes",
                ),
                hunks: &[2],
                restore: Content::text(concat!(
                    "First [start](/uno.md) here.\n",
                    "filler 01\n",
                    "filler 02\n",
                    "filler 03\n",
                    "filler 04\n",
                    "filler 05\n",
                    "filler 06\n",
                    "filler 07\n",
                    "filler 08\n",
                    "filler 09\n",
                    "filler 10\n",
                    "Then [middle](/two.md) follows.\n",
                )),
            },
        ]),
        reader: concat!(
            "Two destination changes in different passages are separate ",
            "decisions; each is exposed on its own.",
        ),
    },
    Fixture {
        id: "links-autolink-to-inline",
        title: "An autolink becomes an inline link",
        classes: &["links"],
        path: "doc.md",
        before: Side::present_text("Visit <https://example.invalid> today.\n", 0o644),
        after: Side::present_text("Visit [the site](https://example.invalid) today.\n", 0o644),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 1, 1, 1)],
        },
        expectation: Expectation::groups(&[Group {
            reason: "coupled link group: the link syntax changes, not the destination",
            hunks: &[1],
            restore: Content::text("Visit <https://example.invalid> today.\n"),
        }]),
        reader: concat!(
            "The destination is unchanged while the syntax changes; the source ",
            "view makes the rewrite explicit.",
        ),
    },
    Fixture {
        id: "links-image-alt",
        title: "An image reference changes its alternative text",
        classes: &["links"],
        path: "doc.md",
        before: Side::present_text("![old chart](chart.png)\n", 0o644),
        after: Side::present_text("![revised chart](chart.png)\n", 0o644),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 1, 1, 1)],
        },
        expectation: Expectation::groups(&[Group {
            reason: "coupled link group: the image label changes, the target does not",
            hunks: &[1],
            restore: Content::text("![old chart](chart.png)\n"),
        }]),
        reader: concat!(
            "The image is shown as a labeled source reference and is never ",
            "fetched; only the alternative text changes.",
        ),
    },
    Fixture {
        id: "reference-links-target",
        title: "A reference definition changes its target",
        classes: &["reference-links"],
        path: "doc.md",
        before: Side::present_text(
            concat!("See the [guide][1] first.\n", "\n", "[1]: /old.md\n",),
            0o644,
        ),
        after: Side::present_text(
            concat!("See the [guide][1] first.\n", "\n", "[1]: /new.md\n",),
            0o644,
        ),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 3, 1, 3)],
        },
        expectation: Expectation::whole_file(concat!(
            "reference-link relation: a definition change can affect every use of ",
            "the label, so independence is not established from both versions",
        )),
        reader: concat!(
            "The definition change and its affected references are disclosed ",
            "together; the whole document is the decision.",
        ),
    },
    Fixture {
        id: "reference-links-definition-added",
        title: "A second reference definition is added",
        classes: &["reference-links"],
        path: "doc.md",
        before: Side::present_text(
            concat!("The [guide][1] opens.\n", "\n", "[1]: /one.md\n",),
            0o644,
        ),
        after: Side::present_text(
            concat!(
                "The [guide][1] opens.\n",
                "\n",
                "[1]: /one.md\n",
                "[2]: /two.md\n",
            ),
            0o644,
        ),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 3, 1, 4)],
        },
        expectation: Expectation::whole_file(concat!(
            "reference-link relation: a new definition can bind future ",
            "references, so independence is not established",
        )),
        reader: concat!(
            "A new definition may bind references elsewhere in the document; the ",
            "conservative unit is the whole document.",
        ),
    },
    Fixture {
        id: "reference-links-shared-definition",
        title: "Two references share one changed definition",
        classes: &["reference-links"],
        path: "doc.md",
        before: Side::present_text(
            concat!(
                "The [guide][g] opens. Later the [guide][g] repeats.\n",
                "\n",
                "[g]: /old.md\n",
            ),
            0o644,
        ),
        after: Side::present_text(
            concat!(
                "The [guide][g] opens. Later the [guide][g] repeats.\n",
                "\n",
                "[g]: /new.md\n",
            ),
            0o644,
        ),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 3, 1, 3)],
        },
        expectation: Expectation::whole_file(concat!(
            "reference-link relation: one definition serves two references, so ",
            "the affected uses cannot be separated",
        )),
        reader: concat!(
            "Both affected references are disclosed with the definition change; ",
            "restoring only one use would mislead.",
        ),
    },
    Fixture {
        id: "footnotes-body-edit",
        title: "A footnote body changes",
        classes: &["footnotes"],
        path: "doc.md",
        before: Side::present_text(
            concat!(
                "The claim is narrow.[^1]\n",
                "\n",
                "[^1]: First measurement.\n",
            ),
            0o644,
        ),
        after: Side::present_text(
            concat!(
                "The claim is narrow.[^1]\n",
                "\n",
                "[^1]: Second measurement.\n",
            ),
            0o644,
        ),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 3, 1, 3)],
        },
        expectation: Expectation::whole_file(concat!(
            "footnote relation: the marker and its definition form one relation ",
            "that cannot be separated",
        )),
        reader: concat!(
            "The footnote body and its marker are disclosed together; the whole ",
            "document is the decision.",
        ),
    },
    Fixture {
        id: "footnotes-marker-added",
        title: "A second footnote marker and definition appear",
        classes: &["footnotes"],
        path: "doc.md",
        before: Side::present_text(
            concat!(
                "One note exists.[^1]\n",
                "filler 01\n",
                "filler 02\n",
                "filler 03\n",
                "filler 04\n",
                "filler 05\n",
                "filler 06\n",
                "filler 07\n",
                "filler 08\n",
                "filler 09\n",
                "filler 10\n",
                "[^1]: body of the note.\n",
            ),
            0o644,
        ),
        after: Side::present_text(
            concat!(
                "One note exists.[^1] A second arrives.[^2]\n",
                "filler 01\n",
                "filler 02\n",
                "filler 03\n",
                "filler 04\n",
                "filler 05\n",
                "filler 06\n",
                "filler 07\n",
                "filler 08\n",
                "filler 09\n",
                "filler 10\n",
                "[^1]: body of the note.\n",
                "[^2]: extra note.\n",
            ),
            0o644,
        ),
        changes: ExpectedChanges {
            change: FileChange::Modified,
            binary: false,
            hunks: &[HunkRange::new(1, 4, 1, 4), HunkRange::new(10, 3, 10, 4)],
        },
        expectation: Expectation::whole_file(concat!(
            "footnote relation: the marker and its definition change together in ",
            "two distant hunks",
        )),
        reader: concat!(
            "A new marker and its definition form one relation; reverting only ",
            "the marker or only the definition would corrupt the document.",
        ),
    },
];
