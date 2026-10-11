# Bench: corpora, harness, and results

`bench/` holds the benchmark side of SPEC 17: the seeded corpus
generator, the process benchmark harness, the registered runner
environment record, the initial result records, and the manually
curated holdout set.

## Layout

```text
bench/README.md            this overview
bench/corpus/main.rs       the corpusgen example entry point
bench/corpus/gen/          the corpus generator library
bench/perf/main.rs         the benchrun example entry point
bench/perf/                the harness library
bench/environment.json     the runner record (SPEC 17.6), registered to the dedicated runner
bench/results/             one not_measured record per metric and corpus (SPEC 17.8)
bench/results/raw/         raw sample streams of measured runs, untracked
bench/holdout/             the manually curated holdout set (SPEC 17.2)
bench/corpora/             generated corpora, never committed
```

Generator modules:

| Module | Responsibility |
| --- | --- |
| `gen/rng.rs` | The seeded SplitMix64 stream; corpora are pure functions of the seed |
| `gen/runner.rs` | Isolated stock-Git execution with fixed identity and dates |
| `gen/text.rs` | Authored synthetic Markdown; exact byte totals; long/wrapped pairs |
| `gen/edits.rs` | The five edit classes and their hunk separation |
| `gen/plan.rs` | The C1-C4 definitions, scaling, history layout, document plan |
| `gen/build.rs` | Repository construction, C4 variants, packing, measurement |
| `gen/manifest.rs` | The manifest record types |

## Corpora (SPEC 17.2)

| Corpus | Full-scale shape |
| --- | --- |
| C1 — daily desk | 100 documents of 20 KiB; 200 commits; 10 changed documents; selected 20 KiB document with 10 changes |
| C2 — release envelope | 1,000 documents totaling exactly 100 MiB; 1,000 commits; 50 changed documents totaling 5 MiB; selected 100 KiB document with 50 changes; maximum file 1 MiB |
| C3 — stress | 10,000 documents totaling exactly 1 GiB; 10,000 commits; an included 10 MiB single-paragraph document and repeated-line pathological cases |
| C4 — Git-state variants | Independent C1-shaped repositories: staged content, untracked/ignored files, add/delete, hooks, conflicts, storage policy, SHA-256, and loose packing |

Every corpus repository has the same structure: a `writing/` scope root
with `.md` suffix, one root `start` marker holding only the scoped
documents, one external commit with out-of-scope sentinels, a linear
history of draft markers broken by keep-all (empty) and selective review
markers, and — after the last review — one draft per changed document.
The distance from the candidate `HEAD` to the last review is therefore
exactly the changed-document count. The adopted `margin.*` settings are
written, so `margin status` and `margin diff` work on a corpus as
shipped.

The history's review markers are built with the product's own
`CommitMetadata` types, so the corpora are structurally valid Margin
history (SPEC 8.3-8.4), not just Git commits that look similar.

## Running the generator

```sh
# One corpus at the quick test scale
cargo run --example corpusgen -- --corpus c1 --output bench/corpora/c1

# The exact SPEC sizes; c3 needs about 1 GiB plus its object store
# and on the order of an hour on a laptop-class machine (10,000 commits)
cargo run --example corpusgen -- --corpus c1 --scale full
cargo run --example corpusgen -- --corpus c2 --scale full
cargo run --example corpusgen -- --corpus c3 --scale full

# Every C4 Git-state variant
cargo run --example corpusgen -- --corpus c4 --output bench/corpora/c4

# One variant, an explicit seed, loose packing, or SHA-256
cargo run --example corpusgen -- --corpus c4 --variant conflict
cargo run --example corpusgen -- --corpus c1 --seed 20261011
cargo run --example corpusgen -- --corpus c1 --packing loose
cargo run --example corpusgen -- --corpus c1 --object-format sha256
```

The generator never overwrites an existing output directory; regenerate
into a new path or remove the old one.

Each build writes:

```text
<output>/repo          the corpus repository
<output>/gen-home      the private fixture HOME used during generation
<output>/manifest.json the record of the run
```

`gen-home/` is scaffolding; the repository and the manifest are the
fixture. Delete `gen-home/` freely after generation.

## The manifest

Every run records, in `manifest.json`:

| Section | Recorded facts |
| --- | --- |
| identity | corpus, seed, generator revision, full parameters |
| `layout` | documents, exact byte totals, largest document, commits, changed documents, the selected document and its change count |
| `graph` | linear shape, commit counts by kind, empty-review count, selective-review count, distance to the last review, candidate and baseline commits and trees |
| `content` | the scoped content digest of the candidate (SPEC 8.6 encoding, Git hashing), and one record per changed document: kind, edit classes, hunk count, before/after sizes and blob IDs, and the Git hash of the exact patch bytes |
| `expected` | keep-all and restore-all result trees, changed-document and change-group totals, out-of-scope sentinels |
| `storage` | object format, requested packing state, settings presence |
| `variant` | the C4 extra: name, description, and observable facts an oracle can check |
| `observed` | Git version, loose and packed object counts, exact `.git` byte total |

`observed` measures the environment and is excluded from the
reproducibility comparison; everything else must be byte-identical
across reruns, and `tests/bench_corpora.rs` proves it for every corpus.

Two reproducibility details are deliberate:

- All commits use a fixed fixture identity and a commit clock that
  advances one minute per commit, so object IDs never depend on the wall
  clock.
- The packing step runs `git repack` with `pack.threads=1`: Git's
  parallel delta search chooses different delta bases run to run, which
  would make identical object sets produce different pack bytes and a
  different `.git` size.

## Scales and verification

The full definitions are the SPEC 17.2 numbers and stay fixed. The test
scale shrinks every dimension through the same code paths so the
developer suite runs in seconds. Measured full-scale runs on the
4-core developer machine (single-threaded repack included): C1 about
4 seconds, C2 about 60 seconds, C3 about 51 minutes and 2 GiB of
generator memory for its 10,000 commits.

```sh
cargo test --test bench_corpora            # every corpus at test scale
cargo test --test bench_harness            # the harness acceptance suite
cargo test --example corpusgen             # just the generator's unit tests
cargo test --example benchrun              # the harness's unit tests
MARGIN_BENCH_FULL_C1=1 cargo test --test bench_corpora   # exact sizes
MARGIN_BENCH_FULL_C2=1 cargo test --test bench_corpora
MARGIN_BENCH_FULL_C3=1 cargo test --test bench_corpora   # slow: 1 GiB, about an hour
```

The harness suite generates its own small corpora under the run's
`LOOPER_RESOURCE_PREFIX` scratch space, registers a probe record, and
runs real measurements through the debug executable: shortened,
diagnostic runs whose verdicts are honestly `failed` or `inconclusive`.

Plain `cargo test` runs the generator's unit tests too, because they live
in the example target.

The suite's oracle is stock Git plus raw bytes (SPEC 16.3): it
re-derives commit counts, marker kinds, empty reviews, exact sizes, the
changed set, hunk counts under the fixed diff contract, and the recorded
variant states, and it reruns generation to prove the manifest
reproduces. Marker validity is additionally checked through the
product's own recognizer, because the corpora are Margin fixtures; that
check establishes fixture validity, not product correctness.

## The process benchmark harness (SPEC 17.3-17.8)

`cargo run --example benchrun` drives the measurement side. Its modules:

| Module | Responsibility |
| --- | --- |
| `perf/envrec.rs` | The runner record: probing actual hardware, software, build, tools, power, terminal facts |
| `perf/metric.rs` | The SPEC 17.4 metric table with its budgets |
| `perf/timing.rs` | The SPEC 17.3 boundary: before process creation through exit and full response delivery |
| `perf/fixture.rs` | Corpus preparation and the mutation reset every iteration starts from |
| `perf/oracle.rs` | Independent stock-Git oracles, including the index plumbing that derives expected result trees |
| `perf/samples.rs` | Raw paired samples: JSON Lines, warmups and slow runs retained |
| `perf/stats.rs` | Nearest-rank median/p95 and the 10,000-resample paired bootstrap |
| `perf/record.rs` | The SPEC 17.8 result record and its statuses |
| `perf/pty.rs` | The pseudo-terminal driver of the P-OPEN, P-KEY, and P-REFRESH sessions |
| `perf/resource.rs` | The P-CPU, P-RSS, and P-TREE-MEM samplers over `/proc` |
| `perf/runner.rs` | The SPEC 17.7 run loop, the PTY and resource dispatch, and the block invalidation rule |
| `perf/hash.rs` | Self-contained SHA-256 for executable, lock, and manifest hashes |

The workflow:

```sh
# 1. Register the runner (SPEC 17.6). The committed record holds the
#    actual dedicated runner, probed on the machine that measures; a
#    re-registration replaces it, and nothing is transcribed by hand.
cargo run --example benchrun -- register --id the-laptop \
    --conditions "on AC power, governor performance, no competing builds"

# 2. Write the initial not_measured records if they are absent.
cargo run --example benchrun -- init-results

# 3. Time one metric against one corpus. The corpus is generated at
#    test scale when absent; pass --scale full for the SPEC sizes.
cargo run --example benchrun -- run --metric P-STATUS --corpus c1 \
    --executable target/release/margin

# 4. Compare a candidate against a baseline under the paired policy.
cargo run --example benchrun -- run --metric P-DIFF --corpus c2 \
    --baseline target/release/margin-base --candidate target/release/margin

# 5. A documented incident invalidates a whole block; samples stay.
cargo run --example benchrun -- invalidate --samples <samples.jsonl> \
    --block 3 --reason "build storm" --record bench/results/P-STATUS.C1.json
```

Defaults follow SPEC 17.7.3 exactly: five warmups and twenty independent
blocks of five fresh-process runs per version. Shortened runs are
diagnostic: their records come back `inconclusive` with a note, never
`passed`.

What the harness guarantees:

- **The boundary (17.3).** The clock starts immediately before `spawn`
  and stops after both pipes drain and the child is reaped, so Git
  children, parsing, verification, and serialization are all inside.
  Warm CLI means a fresh process with warmed caches; the method is
  recorded in every sample.
- **The reset (17.7.2).** Every iteration — warmups included — starts
  from `git reset --hard` to the candidate C plus the exact dirty bytes
  for P-CHECKPOINT or a freshly numbered review plan for P-APPLY, and
  the pre-state is asserted before the clock starts, so a run cannot
  degrade into a no-op or a receipt lookup.
- **The oracles (17.7.6).** Each run is verified through stock Git:
  reported heads and baselines, manifest group totals, the moved
  commit's parent, trailer block, and — for the selective review — a
  result tree derived by independent index plumbing from the manifest's
  blob IDs. A fast incorrect command fails the benchmark.
- **The samples (17.7.3-17.7.4).** Both versions' runs land in one raw
  stream with version, block, order index, oracle outcome, and the
  recorded order seed. Slow samples and warmups are retained; block
  invalidation marks and preserves.
- **The verdicts (17.1, 17.8).** Records distinguish `not_measured`,
  `passed`, `failed`, and `inconclusive`. Until a runner registers,
  measurement refuses and the records stay `not_measured`.

The PTY metrics (P-OPEN, P-KEY, P-REFRESH) and the resource metrics
(P-CPU, P-RSS, P-TREE-MEM) have samplers of their own:

- **The session shape.** Every measured session runs the surface on a
  fresh 120-column, 40-row `xterm-256color` pseudo-terminal, with the
  fixture repository as the working directory and `--reduced-updates`
  on, so bytes arriving at the master mean the surface answered. The
  driver calls only the POSIX terminal set through the C library the
  standard library already links; no dependency enters the tree, and a
  session that ignores its limits is killed, never left hanging.
- **The PTY boundaries (17.4).** P-OPEN runs from immediately before
  the spawn until the first frame that shows identity, content, save
  state, and actions. P-KEY runs from immediately before one key's
  bytes are written until the first answering byte, and every event of
  the deterministic `j`/`k` script is one sample row. P-REFRESH runs
  from an external edit's write returning until the arrival notice
  flushes.
- **The resource statistics (17.4, 17.5).** P-RSS reads the parent's
  `VmHWM`; P-TREE-MEM samples the concurrent process-tree sum of
  `VmRSS` every 20 ms; P-CPU accumulates `utime+stime` over the tree
  across one idle window. Every session drives the same fixed
  open/review/history sequence, and each record's `memory_definition`
  states what was summed, from where, in which unit.
- **Their gates.** A latency budget reads the nearest-rank p95, P-KEY
  adds its p99 once the event series is large enough to carry one, and
  a resource ceiling reads the maximum counted sample, never a
  percentile. A resource record keeps its base latency fields empty;
  its numbers live in `details.statistics` with a method sentence that
  names the unit.

Paired comparison stays a CLI-only shape: the SPEC 17.7 regression
gate is defined over paired fresh-process blocks, and a PTY or
resource pairing would need its own design. The `run` command refuses
a paired plan for these metrics explicitly; measure one side with
`--executable`.

```sh
# A PTY metric: the sampler drives the real surface on a real terminal.
cargo run --example benchrun -- run --metric P-KEY --corpus c1 \
    --executable target/release/margin

# The resource metrics pin their own policy knobs: P-CPU takes two
# five-minute idle windows by default, the memory metrics take one
# warmup and 4 blocks of 3 runs, and P-KEY takes 100 events per session.
cargo run --example benchrun -- run --metric P-TREE-MEM --corpus c1 \
    --executable target/release/margin
cargo run --example benchrun -- run --metric P-CPU --corpus c1 \
    --executable target/release/margin --window-seconds 300
```

## Holdout set

`bench/holdout/` is the small manually curated set SPEC 17.2 requires:
hand-authored pairs that are **not used to tune the current
optimization**. See `bench/holdout/README.md` for the protocol before
touching it.

## Content provenance

All corpus text is authored synthetic prose generated from the seeded
stream — word lists written for this project, no private drafts, no
copied or licensed content (SPEC 17.2). The holdout pairs are
hand-written to the same rule.
