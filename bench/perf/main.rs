//! The benchmark harness entry point (SPEC 17.3-17.8).
//!
//! Build and run it through Cargo; it is a development example, never a
//! shipped executable:
//!
//! ```text
//! cargo run --example benchrun -- list
//! cargo run --example benchrun -- init-results
//! cargo run --example benchrun -- register --id the-laptop
//! cargo run --example benchrun -- run --metric P-STATUS --corpus c1 \
//!     --executable target/release/margin
//! cargo run --example benchrun -- run --metric P-DIFF --corpus c2 \
//!     --baseline target/release/margin-base --candidate target/release/margin
//! cargo run --example benchrun -- run --metric P-KEY --corpus c1 \
//!     --executable target/release/margin
//! cargo run --example benchrun -- invalidate --samples <samples.jsonl> \
//!     --block 3 --reason "compiler build storm" --record <P-*.json>
//! ```
//!
//! `run` refuses to measure until `register` has filled
//! `bench/environment.json` with an actual runner (SPEC 17.6); until
//! then every result stays `not_measured` (SPEC 17.8).

use std::path::PathBuf;
use std::process::ExitCode;

#[path = "mod.rs"]
mod perf;

use perf::envrec::{EnvironmentRecord, probe_record};
use perf::metric;
use perf::record::ResultRecord;
use perf::runner::{RunPlan, Sides, require_registered};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("benchrun: {message}");
            eprintln!("run with --help for usage");
            ExitCode::FAILURE
        }
    }
}

/// The default paths the operator-facing subcommands use.
const DEFAULT_ENVIRONMENT: &str = "bench/environment.json";
const DEFAULT_RESULTS: &str = "bench/results";

fn usage() {
    println!(
        "benchrun — the Margin process benchmark harness (SPEC 17.3-17.8)\n\
         \n\
         USAGE:\n\
         \tcargo run --example benchrun -- COMMAND [OPTIONS]\n\
         \n\
         COMMANDS:\n\
         \tlist                       List the primary metrics and budgets\n\
         \tinit-results               Write the initial not_measured result records\n\
         \tregister                   Probe this machine and register it as the runner\n\
         \trun                        Time one metric against one corpus\n\
         \tinvalidate                 Mark one block invalidated and recompute the record\n\
         \n\
         init-results OPTIONS:\n\
         \t--results DIR       Results directory (default {DEFAULT_RESULTS})\n\
         \n\
         register OPTIONS:\n\
         \t--output FILE       Environment record (default {DEFAULT_ENVIRONMENT})\n\
         \t--id ID             Runner identifier (default derived from the machine)\n\
         \t--probe-path DIR    Path whose storage is recorded (default .)\n\
         \t--conditions TEXT   Operator statement of power and interference\n\
         \n\
         run OPTIONS:\n\
         \t--metric ID         One of the P-* identifiers (required)\n\
         \t--corpus c1|c2|c3   Which corpus budget applies (required)\n\
         \t--corpus-dir DIR    Existing corpus root with repo/ and manifest.json;\n\
         \t                    generated at test scale when absent\n\
         \t--output DIR        Where an absent corpus is generated\n\
         \t                    (default bench/corpora/<corpus>)\n\
         \t--scale test|full   Scale of a generated corpus (default test)\n\
         \t--executable PATH   One absolute measurement against the budget\n\
         \t--baseline PATH     Paired baseline executable\n\
         \t--candidate PATH    Paired candidate executable\n\
         \t--environment FILE  Environment record (default {DEFAULT_ENVIRONMENT})\n\
         \t--results DIR       Results directory (default {DEFAULT_RESULTS})\n\
         \t--warmups N         Warmup runs per version (default 5, SPEC 17.7.3;\n\
         \t                    0 for P-CPU, 1 for P-RSS and P-TREE-MEM)\n\
         \t--blocks N          Independent paired blocks (default 20; 2 for\n\
         \t                    P-CPU, 4 for P-RSS and P-TREE-MEM)\n\
         \t--runs-per-block N  Fresh-process runs per block (default 5; 1 for\n\
         \t                    P-CPU, 3 for P-RSS and P-TREE-MEM)\n\
         \t--events-per-session N  Counted navigation events per P-KEY\n\
         \t                    session (default 100; SPEC 17.4's policy of\n\
         \t                    20 blocks x 5 sessions x 100 events reaches\n\
         \t                    the 10,000-event floor)\n\
         \t--window-seconds N  Length of one P-CPU idle window (default 300,\n\
         \t                    the five-minute definition of SPEC 17.4; a\n\
         \t                    shorter window is diagnostic and reads\n\
         \t                    inconclusive)\n\
         \t--seed N            Order-randomization seed, recorded (default 1)\n\
         \t--commit OID        Implementation commit of the executable\n\
         \t--note TEXT         Recorded verbatim in the result record\n\
         \n\
         invalidate OPTIONS:\n\
         \t--samples FILE      The raw sample stream to mark\n\
         \t--block N           The block invalidated by a documented incident\n\
         \t--reason TEXT       The documented reason (required)\n\
         \t--record FILE       The result record to recompute\n\
         \n\
         --help                Show this help"
    );
}

/// The parsed command line.
struct Options {
    command: String,
    values: Vec<(String, String)>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut rest = args.iter();
    let command = rest
        .next()
        .ok_or_else(|| "a command is required".to_string())?
        .to_string();
    let mut values = Vec::new();
    let stream: Vec<String> = rest.cloned().collect();
    let mut index = 0;
    while index < stream.len() {
        let argument = stream[index].clone();
        if argument == "--help" || argument == "-h" {
            usage();
            std::process::exit(0);
        }
        let takes_value = [
            "--metric",
            "--corpus",
            "--corpus-dir",
            "--output",
            "--scale",
            "--executable",
            "--baseline",
            "--candidate",
            "--environment",
            "--results",
            "--warmups",
            "--blocks",
            "--runs-per-block",
            "--events-per-session",
            "--window-seconds",
            "--seed",
            "--commit",
            "--note",
            "--id",
            "--probe-path",
            "--conditions",
            "--samples",
            "--block",
            "--reason",
            "--record",
        ]
        .contains(&argument.as_str());
        if takes_value {
            index += 1;
            let value = stream
                .get(index)
                .cloned()
                .ok_or_else(|| format!("{argument} needs a value"))?;
            values.push((argument, value));
        } else {
            return Err(format!("unexpected argument {argument}"));
        }
        index += 1;
    }
    Ok(Options { command, values })
}

impl Options {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.value(name)
            .map(|value| value.to_string())
            .ok_or_else(|| format!("{name} is required"))
    }

    fn number(&self, name: &str, default: usize) -> Result<usize, String> {
        match self.value(name) {
            None => Ok(default),
            Some(raw) => raw
                .parse()
                .map_err(|_| format!("{name} needs a number: {raw}")),
        }
    }

    fn notes(&self) -> Vec<String> {
        self.values
            .iter()
            .filter(|(key, _)| key == "--note")
            .map(|(_, value)| value.clone())
            .collect()
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = parse(&args)?;
    match options.command.as_str() {
        "list" => list(),
        "init-results" => init_results(&options),
        "register" => register(&options),
        "run" => measure(&options),
        "invalidate" => invalidate(&options),
        other => Err(format!("unknown command {other}")),
    }
}

/// Lists the metric table of SPEC 17.4.
fn list() -> Result<(), String> {
    println!("primary metrics (SPEC 17.4):");
    for definition in metric::ALL {
        println!(
            "  {:<13} {:<18} C1: {:<28} C2: {}",
            definition.id,
            definition.kind.as_str(),
            definition.c1.describe(),
            definition.c2.describe()
        );
    }
    Ok(())
}

/// Writes the initial `not_measured` records for every metric and
/// budget-bearing corpus (SPEC 17.1: the first results are
/// `not_measured`, never zero or presumed passing).
fn init_results(options: &Options) -> Result<(), String> {
    let results = options
        .value("--results")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RESULTS));
    std::fs::create_dir_all(&results)
        .map_err(|error| format!("cannot create {}: {error}", results.display()))?;
    let mut count = 0;
    for definition in metric::ALL {
        for corpus in metric::RECORD_CORPORA {
            if definition.budget(corpus).is_none() {
                continue;
            }
            let record = ResultRecord::not_measured(definition.id, corpus);
            let path = results.join(ResultRecord::file_name(definition.id, corpus));
            record
                .write(&path)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            count += 1;
        }
    }
    println!(
        "benchrun: wrote {count} not_measured records into {}",
        results.display()
    );
    Ok(())
}

/// Probes this machine and registers it as the runner (SPEC 17.6).
fn register(options: &Options) -> Result<(), String> {
    let output = options
        .value("--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ENVIRONMENT));
    let probe_path = options
        .value("--probe-path")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let source_root = std::env::current_dir()
        .ok()
        .filter(|cwd| cwd.join("Cargo.lock").exists());
    let record = probe_record(
        options.value("--id"),
        &probe_path,
        source_root.as_deref(),
        options.value("--conditions"),
    );
    record
        .write(&output)
        .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
    println!(
        "benchrun: registered {} as runner {} ({}); record: {}",
        record
            .observed_class
            .as_deref()
            .unwrap_or("an unclassified machine"),
        record.runner_id.as_deref().unwrap_or("?"),
        if record.matches_declared_class == Some(true) {
            "matches the declared developer-laptop-class x86-64 target"
        } else {
            "does not match the declared class; record this honestly"
        },
        output.display()
    );
    Ok(())
}

/// Generates the corpus at `output` when it is absent, at test or full
/// scale, and returns its root.
fn ensure_corpus(options: &Options) -> Result<PathBuf, String> {
    if let Some(existing) = options.value("--corpus-dir") {
        let root = PathBuf::from(existing);
        if !root.join("manifest.json").exists() {
            return Err(format!(
                "{} holds no manifest.json; generate the corpus with corpusgen first",
                root.display()
            ));
        }
        return Ok(root);
    }
    let corpus = options.required("--corpus")?;
    let output = options
        .value("--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("bench/corpora/{}", corpus.to_ascii_lowercase())));
    if output.join("manifest.json").exists() {
        return Ok(output);
    }
    let full = match options.value("--scale").unwrap_or("test") {
        "test" => false,
        "full" => true,
        other => return Err(format!("unknown scale {other}")),
    };
    #[path = "../corpus/gen/mod.rs"]
    mod corpus;
    let base = match corpus.as_str() {
        "c1" => corpus::Parameters::c1(),
        "c2" => corpus::Parameters::c2(),
        "c3" => corpus::Parameters::c3(),
        other => return Err(format!("unknown corpus {other}")),
    };
    let parameters = if full { base } else { base.test_scale() };
    println!(
        "benchrun: generating {} at {} scale in {} (outside timing)",
        parameters.corpus,
        if full { "full" } else { "test" },
        output.display()
    );
    let built = corpus::generate(&parameters, &output);
    println!(
        "benchrun: corpus ready — {} documents, {} commits, distance to last review {}",
        built.manifest.layout.documents,
        built.manifest.layout.commits,
        built.manifest.graph.distance_to_last_review
    );
    Ok(output)
}

/// Times one metric (SPEC 17.3, 17.7).
fn measure(options: &Options) -> Result<(), String> {
    let metric_id = options.required("--metric")?;
    let corpus = options.required("--corpus")?.to_ascii_uppercase();
    let corpus_list = metric::RECORD_CORPORA.join(", ");
    let definition = metric::by_id(&metric_id)
        .ok_or_else(|| format!("unknown metric {metric_id}; run `list` for the table"))?;
    let environment_path = options
        .value("--environment")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ENVIRONMENT));
    let environment = EnvironmentRecord::load(&environment_path)
        .map_err(|error| format!("cannot read the environment record: {error}"))?;
    require_registered(&environment)?;

    let results = options
        .value("--results")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RESULTS));
    std::fs::create_dir_all(&results)
        .map_err(|error| format!("cannot create {}: {error}", results.display()))?;
    // The scratch indexes and raw sample streams derived from this
    // directory reach Git children as `GIT_INDEX_FILE` and file paths,
    // and those children run with the fixture repository as their
    // working directory, so a relative directory would resolve against
    // the repository instead of this process's tree.
    let results = results
        .canonicalize()
        .map_err(|error| format!("{} cannot be resolved: {error}", results.display()))?;

    // The children run with the fixture repository as their working
    // directory, so a relative executable path would resolve against it;
    // resolve every executable against this process first.
    let resolve = |raw: &str| -> Result<PathBuf, String> {
        let path = PathBuf::from(raw);
        path.canonicalize()
            .map_err(|error| format!("{} cannot be resolved: {error}", path.display()))
    };
    let sides =
        match (
            options.value("--executable"),
            options.value("--baseline"),
            options.value("--candidate"),
        ) {
            (Some(single), None, None) => Sides::Single(resolve(single)?),
            (None, Some(baseline), Some(candidate)) => Sides::Paired {
                baseline: resolve(baseline)?,
                candidate: resolve(candidate)?,
            },
            _ => return Err(
                "pass either --executable for one absolute measurement, or both --baseline and \
                 --candidate for a paired comparison"
                    .to_string(),
            ),
        };

    let corpus_root = ensure_corpus(options)?;
    let scratch = results.join(format!("scratch.{}.{}", metric_id, corpus));
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
    let fixture = perf::fixture::Fixture::prepare(&corpus_root, &scratch)?;

    metric::RECORD_CORPORA
        .iter()
        .find(|label| **label == corpus)
        .ok_or_else(|| format!("unknown corpus {corpus}; expected one of {corpus_list}"))?;
    // The sampling defaults follow the metric kind (SPEC 17.4, 17.7.3):
    // the CLI and PTY latency metrics take the 5/20/5 policy; P-KEY adds
    // its per-session event count; P-CPU takes two idle windows; the
    // memory metrics take one warmup and 4 blocks of 3 runs.
    let (warmups, blocks, runs_per_block) = match definition.id {
        "P-CPU" => (0, 2, 1),
        "P-RSS" | "P-TREE-MEM" => (1, 4, 3),
        _ => (5, 20, 5),
    };
    let events_per_session = match definition.id {
        "P-KEY" => options.number("--events-per-session", 100)?,
        _ => 0,
    };
    let window_seconds = match definition.id {
        "P-CPU" => options.number("--window-seconds", 300)? as u64,
        _ => 0,
    };
    let plan = RunPlan {
        metric: definition.id,
        corpus: corpus.clone(),
        sides,
        warmups: options.number("--warmups", warmups)?,
        blocks: options.number("--blocks", blocks)?,
        runs_per_block: options.number("--runs-per-block", runs_per_block)?,
        order_seed: options.number("--seed", 1)? as u64,
        events_per_session,
        window_seconds,
        implementation_commit: implementation_commit(options)?,
        notes: options.notes(),
    };

    let outcome = perf::runner::run(&plan, &fixture, &environment, &environment_path, &results)?;
    // A resource record keeps its base latency fields empty on purpose;
    // its summary quotes the maximum counted sample in its own unit.
    let summary = match outcome.record.details.as_ref().and_then(|details| {
        details
            .statistics
            .as_ref()
            .filter(|_| definition.kind == metric::Kind::Resource)
    }) {
        Some(statistics) => format!(
            "max {:.1} {} against the ceiling",
            statistics.max_ms,
            match definition.id {
                "P-CPU" => "percent-of-one-core",
                _ => "MiB",
            }
        ),
        None => format!(
            "median {:.1} ms, p95 {:.1} ms, budget {}",
            outcome.record.median_ms.unwrap_or(0.0),
            outcome.record.p95_ms.unwrap_or(0.0),
            outcome
                .record
                .budget_p95_ms
                .map(|ms| format!("{ms} ms"))
                .unwrap_or_else(|| "n/a".to_string()),
        ),
    };
    println!(
        "benchrun: {} on {} — {} ({} counted samples, {})",
        plan.metric,
        corpus,
        outcome.record.status.as_str(),
        outcome.record.sample_count,
        summary,
    );
    println!(
        "benchrun: oracle {}, raw samples {}",
        outcome.record.correctness_oracle.as_str(),
        outcome.raw_path.display()
    );
    println!("benchrun: record {}", outcome.record_path.display());
    Ok(())
}

/// The implementation commit: the flag when given, else probed from the
/// containing repository.
fn implementation_commit(options: &Options) -> Result<Option<String>, String> {
    if let Some(commit) = options.value("--commit") {
        return Ok(Some(commit.to_string()));
    }
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| format!("cannot probe the implementation commit: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Marks one block invalidated and recomputes the record from the
/// retained samples (SPEC 17.7.4: preserve the block, note the
/// incident).
fn invalidate(options: &Options) -> Result<(), String> {
    let samples_path = PathBuf::from(options.required("--samples")?);
    let block: usize = options
        .required("--block")?
        .parse()
        .map_err(|_| "the block must be a number")?;
    let reason = options.required("--reason")?;
    let record_path = PathBuf::from(options.required("--record")?);
    perf::runner::invalidate_block(&samples_path, &record_path, block, &reason)
}
