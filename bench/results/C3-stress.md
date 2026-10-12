# C3 stress exercise report (T054, SPEC 17.5)

Corpus: generated full-scale C3 — 10,000 documents totaling 1 GiB,
10,000 commits, one 10 MiB single-paragraph document, eight 64 KiB
repeated-line documents, packed storage, seed `marg…0003` (generator
`margin-corpusgen/1`). Runner: `the-developer-vm`, the registered
runner of `bench/environment.json`.

Raw evidence: `bench/results/raw/C3-stress/<timestamp>/` — one `.out`
and `.err` per observation and an `index.txt` with the wall time, exit,
and bounded stdout size of each command. C3 carries no latency budget
(SPEC 17.5); the wall times record behavior, never speed.

## What the exercise ran

`bench/stress-exercise.sh` against the corpus repository: `status`,
`log --limit 50`, `diff --path <selected>`, whole-scope `diff`, `show`
of the selected document, of the 10 MiB single-paragraph document, and
of every repeated-line document, `diff --limit 10`, and a final
`git status --porcelain`.

## Findings

| Command | Exit | Wall | Bounded stdout | Behavior |
| --- | --- | --- | --- | --- |
| `status --json` | 0 | 1.4 s | 1,392 B | Full workspace; grouping explicitly unavailable |
| `log --limit 50 --json` | 0 | 1.6 s | 19,301 B | Bounded page, no flood |
| `show` selected | 0 | 29 ms | 134,192 B | Content inside the byte cap |
| `show` 10 MiB paragraph | 0 | 113 ms | 10,486,336 B | Whole document, inside the 16 MiB document limit |
| `show` repeated-line docs | 0 | 21-51 ms | 66,838 B each | Whole documents |
| every `diff` variant | 2 | 1.5-1.7 s | 2,333 B each | Typed `LIMIT_EXCEEDED`, non-retryable |
| `git status --porcelain` | 0 | 26 ms | 0 B | The repository is untouched |

- No command panicked, hung, or wrote unbounded output. The slowest
  observation stayed under 1.8 s; every failure is one typed envelope
  whose captured output stayed bounded.
- `status` states its omission: `pending_files: 100` with
  `pending_groups_reason: "grouping_unavailable"` — the group count a
  whole-scope comparison would need is absent by name, never silently.
- Every `diff` variant — including `diff --limit 10` and
  `diff --path` of the selected document — fails `LIMIT_EXCEEDED`: the
  fixed diff contract generates the patch stream for all 100 changed
  paths in batched `diff-tree -p` processes, and that stream — led by
  the 10 MiB single-paragraph document — crosses the 16 MiB per-stream
  capture bound of SPEC 3.3/6.1 before any page could be cut from it.
  No content was approved or displayed from the refused stream.
- Reads touched nothing: `git status --porcelain` is empty.

## Verdict against SPEC 17.5

The C3 stress row — no unbounded allocation or output, no panic, no
approval of omitted data, responsive cancellation and error — holds for
every command this exercise ran. The `diff` refusal is the designed
outcome of the explicit capture bound, and the status summary carries
its omission data explicitly. The path-filtered and page-limited diff
generating the whole-scope patch stream is recorded as follow-up work
(a path-filtered generation would let `diff --path` and `--limit` serve
pages of the C3 shape inside the same bound).
