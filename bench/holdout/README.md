# Benchmark holdout set

SPEC 17.2 requires a small manually curated holdout set that is not used
to tune the current optimization. These document pairs are authored by
hand, reviewed by hand, and kept out of every generator seed and every
performance-tuning loop. They exist so a later evaluation can check a
claim against material nobody optimized against.

## Rules

1. The pairs in this directory are frozen. Do not regenerate, rewrap, or
   "improve" them to match an implementation.
2. No optimization work may read these files. Use them only in a final or
   release evaluation, after the tuning work is complete, and record that
   use in the benchmark result record.
3. Adding a new pair is allowed; changing an existing pair is not. New
   pairs need a reviewer note in `manifest.json`.
4. As with the generated corpora, all text is authored synthetic prose:
   no private drafts and no copied licensed content.

## Layout

- `manifest.json` — the pair inventory with hand-written review notes.
- `pairs/<name>/before.md` and `pairs/<name>/after.md` — the exact B and C
   bytes of one pair.

The current pairs cover the classes the generated corpora exercise most:
a claim change, a link-target change with an unchanged label, and a pure
reflow. Their expected grouping is deliberately not recorded here; the
release evaluation decides it with the product and reviews the outcome.
