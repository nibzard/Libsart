//! The corpus generator entry point (SPEC 17.2).
//!
//! Build and run it through Cargo; it is a development example, never a
//! shipped executable:
//!
//! ```text
//! cargo run --example corpusgen -- --corpus c1 --scale full --output bench/corpora/c1
//! cargo run --example corpusgen -- --corpus c4 --output bench/corpora/c4
//! cargo run --example corpusgen -- --list
//! ```
//!
//! The default scale is `test`, which builds the same shape small enough
//! to verify quickly. The `full` scale materializes the exact sizes of
//! SPEC 17.2 — C3 needs roughly 1 GiB of working-tree content plus its
//! object store.

use std::path::PathBuf;
use std::process::ExitCode;

#[path = "gen/mod.rs"]
mod corpus;

use corpus::{ObjectFormat, Packing, Parameters, Variant};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("corpusgen: {message}");
            eprintln!("run with --help for usage");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    corpus: String,
    scale: String,
    output: Option<PathBuf>,
    seed: Option<u64>,
    packing: Packing,
    object_format: ObjectFormat,
    variant: Option<String>,
}

fn usage() {
    println!(
        "corpusgen — seeded benchmark corpora for Margin (SPEC 17.2)\n\
         \n\
         USAGE:\n\
         \tcargo run --example corpusgen -- [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \t--corpus c1|c2|c3|c4   Which corpus to build (default c1)\n\
         \t--scale test|full      Parameter scale (default test; full uses the\n\
         \t                       exact SPEC 17.2 sizes — c3 needs about 1 GiB)\n\
         \t--output DIR           Output directory (default bench/corpora/<corpus>)\n\
         \t--seed N               Override the recorded default seed\n\
         \t--packing packed|loose Storage packing state (default packed)\n\
         \t--object-format f      sha1 or sha256 (default sha1)\n\
         \t--variant NAME         For c4: build only this variant\n\
         \t--list                 List corpora and variants, then exit\n\
         \t--help                 Show this help"
    );
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        corpus: "c1".to_string(),
        scale: "test".to_string(),
        output: None,
        seed: None,
        packing: Packing::Packed,
        object_format: ObjectFormat::Sha1,
        variant: None,
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let mut value = || -> Result<String, String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{argument} needs a value"))
        };
        match argument {
            "--corpus" => options.corpus = value()?.to_ascii_lowercase(),
            "--scale" => options.scale = value()?.to_ascii_lowercase(),
            "--output" => options.output = Some(PathBuf::from(value()?)),
            "--seed" => {
                options.seed = Some(value()?.parse().map_err(|_| "invalid seed".to_string())?)
            }
            "--packing" => {
                options.packing = match value()?.as_str() {
                    "packed" => Packing::Packed,
                    "loose" => Packing::Loose,
                    other => return Err(format!("unknown packing {other}")),
                }
            }
            "--object-format" => {
                options.object_format = ObjectFormat::parse(&value()?)
                    .ok_or_else(|| "unknown object format".to_string())?
            }
            "--variant" => options.variant = Some(value()?),
            "--list" => {
                list();
                std::process::exit(0);
            }
            "--help" | "-h" => {
                usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
        index += 1;
    }
    Ok(options)
}

fn list() {
    println!("corpora:");
    for (name, describe) in [
        ("c1", "100 x 20 KiB documents, 200 commits, 10 changed"),
        (
            "c2",
            "1000 documents = exactly 100 MiB, 1000 commits, 50 changed = 5 MiB",
        ),
        (
            "c3",
            "10000 documents = 1 GiB, 10000 commits, 10 MiB single paragraph",
        ),
        ("c4", "C1-shaped Git-state variants"),
    ] {
        println!("  {name}  {describe}");
    }
    println!("variants (c4):");
    for variant in Variant::ALL {
        println!("  {:<18} {}", variant.name(), variant.describe());
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = parse(&args)?;
    let full = match options.scale.as_str() {
        "test" => false,
        "full" => true,
        other => return Err(format!("unknown scale {other}")),
    };
    let base = match options.corpus.as_str() {
        "c1" => Parameters::c1(),
        "c2" => Parameters::c2(),
        "c3" => Parameters::c3(),
        "c4" => Parameters::c4_base(),
        other => return Err(format!("unknown corpus {other}")),
    };
    let mut parameters = if full { base } else { base.test_scale() };
    if let Some(seed) = options.seed {
        parameters = parameters.with_seed(seed);
    }
    parameters = parameters.with_packing(options.packing);
    if parameters.corpus != "C4" {
        // The object-format variant is a C4 concern; elsewhere it stays a
        // flag for one-off experiments.
        parameters = parameters.with_object_format(options.object_format);
    }

    if parameters.corpus == "C4" {
        let output = options
            .output
            .unwrap_or_else(|| PathBuf::from("bench/corpora/c4"));
        let variants: Vec<Variant> = match &options.variant {
            Some(name) => {
                let wanted = Variant::ALL
                    .iter()
                    .copied()
                    .find(|variant| variant.name() == name)
                    .ok_or_else(|| format!("unknown variant {name}"))?;
                vec![wanted]
            }
            None => Variant::ALL.to_vec(),
        };
        for variant in variants {
            let target = output.join(variant.name());
            println!(
                "corpusgen: building C4 variant {} in {}",
                variant.name(),
                target.display()
            );
            let built = corpus::generate_variant(variant, &parameters, &target);
            report(&built);
        }
        return Ok(());
    }

    let output = options
        .output
        .unwrap_or_else(|| PathBuf::from(format!("bench/corpora/{}", options.corpus)));
    println!(
        "corpusgen: building {} at {} scale in {}",
        parameters.corpus,
        if full { "full" } else { "test" },
        output.display()
    );
    let built = corpus::generate(&parameters, &output);
    report(&built);
    Ok(())
}

/// Prints one build summary.
fn report(built: &corpus::CorpusResult) {
    let manifest = &built.manifest;
    println!(
        "corpusgen: {} — {} documents, {} bytes, {} commits ({} reviews, {} empty), \
         distance to last review {}, changed {} documents / {} hunks",
        manifest.corpus,
        manifest.layout.documents,
        manifest.layout.total_document_bytes,
        manifest.layout.commits,
        manifest.graph.counts.review,
        manifest.graph.counts.empty_review,
        manifest.graph.distance_to_last_review,
        manifest.layout.changed_documents,
        manifest.expected.changed_groups,
    );
    println!(
        "corpusgen: head {} baseline {} .git {} bytes ({} loose / {} packed objects)",
        &manifest.graph.head[..12.min(manifest.graph.head.len())],
        &manifest.graph.baseline_commit[..12.min(manifest.graph.baseline_commit.len())],
        manifest.observed.git_dir_bytes,
        manifest.observed.loose_objects,
        manifest.observed.in_pack_objects,
    );
    println!("corpusgen: manifest {} manifest.json", built.root.display());
}
