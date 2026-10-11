//! Authored synthetic Markdown for the benchmark corpora (SPEC 17.2).
//!
//! Every document is generated from the seeded stream: no private drafts
//! and no copied text enter the corpora. The module produces ordinary
//! prose paragraphs with embedded claims and links, renders each paragraph
//! either as one long line or hard-wrapped at a fixed width, and pads every
//! document to an exact byte total, so a corpus can state "exactly 100 MiB"
//! and mean it.
//!
//! The same word lists also carry the equal-length swap pairs of
//! [`super::edits`], so a word edit can always find a target.

use super::rng::Rng;

/// Width of hard-wrapped renderings, the common 72-column convention.
pub const WRAP_WIDTH: usize = 72;

/// Nouns of the synthetic vocabulary. Swap-pair members appear here and in
/// [`VERBS`]/[`ADJECTIVES`] so generated prose contains them.
const NOUNS: &[&str] = &[
    "draft", "field", "guide", "notes", "trail", "items", "scope", "record", "margin", "passage",
    "review", "outline", "logbook", "primer", "survey", "tally", "chapter", "notebook",
];

/// Verbs of the synthetic vocabulary.
const VERBS: &[&str] = &[
    "shows", "lists", "records", "holds", "keeps", "marks", "opens", "closes", "joins", "splits",
    "guards", "settles",
];

/// Adjectives of the synthetic vocabulary.
const ADJECTIVES: &[&str] = &[
    "quick", "rapid", "clear", "plain", "sharp", "solid", "early", "prior", "later", "final",
    "steady", "broad", "quiet", "exact",
];

/// Prepositional phrases of the synthetic vocabulary.
const PHRASES: &[&str] = &[
    "about the margin",
    "across the review",
    "before the checkpoint",
    "during the review",
    "inside the outline",
    "beyond the draft",
    "after the restore",
];

/// Subjects the prose revolves around.
const TOPICS: &[&str] = &[
    "margin",
    "review",
    "checkpoint",
    "baseline",
    "passage",
    "grouping",
    "restoration",
    "outline",
];

/// Labels for generated links.
const LABELS: &[&str] = &["guide", "survey", "logbook", "outline", "primer"];

/// Equal-length word pairs for same-length word edits. Both members belong
/// to the vocabulary above.
pub const SWAP_PAIRS: &[(&str, &str)] = &[
    ("quick", "rapid"),
    ("shows", "lists"),
    ("clear", "plain"),
    ("sharp", "solid"),
    ("notes", "items"),
    ("early", "prior"),
    ("later", "final"),
    ("will", "must"),
    ("kept", "held"),
    ("draft", "probe"),
    ("guide", "trail"),
    ("field", "scope"),
];

/// The heading every document opens with, kept out of the wrapped body so
/// the long-paragraph and hard-wrapped renderings of one pair stay visibly
/// the same document.
fn heading(rng: &mut Rng) -> String {
    let topic = TOPICS[rng.below(TOPICS.len())];
    let adjective = ADJECTIVES[rng.below(ADJECTIVES.len())];
    format!("# {adjective} {topic} workbook")
}

/// One plain declarative sentence.
fn plain_sentence(rng: &mut Rng) -> String {
    let noun = |rng: &mut Rng| NOUNS[rng.below(NOUNS.len())];
    let verb = |rng: &mut Rng| VERBS[rng.below(VERBS.len())];
    let adjective = |rng: &mut Rng| ADJECTIVES[rng.below(ADJECTIVES.len())];
    match rng.below(4) {
        0 => format!(
            "The {} {} {} the {} of the {}.",
            adjective(rng),
            noun(rng),
            verb(rng),
            noun(rng),
            TOPICS[rng.below(TOPICS.len())]
        ),
        1 => format!(
            "Writers {} the {} {} {}.",
            verb(rng),
            adjective(rng),
            noun(rng),
            PHRASES[rng.below(PHRASES.len())]
        ),
        2 => format!(
            "The {} {} the {} {} before the review.",
            TOPICS[rng.below(TOPICS.len())],
            verb(rng),
            adjective(rng),
            noun(rng)
        ),
        _ => format!(
            "A {} {} {} {} of the {}.",
            adjective(rng),
            noun(rng),
            verb(rng),
            PHRASES[rng.below(PHRASES.len())],
            noun(rng)
        ),
    }
}

/// One claim sentence: single-digit counts that a claim edit can advance
/// without changing the line length.
fn claim_sentence(rng: &mut Rng) -> String {
    match rng.below(2) {
        0 => format!(
            "The tally records {} of {} open items {}.",
            rng.below(10),
            rng.below(10),
            PHRASES[rng.below(PHRASES.len())]
        ),
        _ => format!(
            "Editors count {} of {} open questions about the {}.",
            rng.below(10),
            rng.below(10),
            TOPICS[rng.below(TOPICS.len())]
        ),
    }
}

/// One sentence carrying a link whose target — not its label — can change.
fn link_sentence(rng: &mut Rng) -> String {
    let label = LABELS[rng.below(LABELS.len())];
    let site = rng.below(1000);
    let topic = TOPICS[rng.below(TOPICS.len())];
    format!("See the [{label}](https://example.invalid/docs/note-{site:03}) for the {topic}.")
}

/// One freshly authored paragraph of `count` plain sentences. The dense
/// edit replaces a paragraph with one of these.
pub fn fresh_paragraph(rng: &mut Rng, count: usize) -> String {
    let sentences: Vec<String> = (0..count).map(|_| plain_sentence(rng)).collect();
    sentences.join(" ")
}

/// The unwrapped draft of one document: a heading plus enough paragraph
/// texts to reach `body_target` characters.
///
/// A draft is rendering-independent, so the long-paragraph document and its
/// hard-wrapped equivalent can hold the same sentences (SPEC 17.2 asks for
/// both forms).
#[derive(Clone, Debug)]
pub struct Draft {
    /// The document heading, one line.
    pub heading: String,
    /// Paragraph texts; each is sentences joined by single spaces.
    pub paragraphs: Vec<String>,
}

/// Builds one draft from `rng` with at least `body_target` characters of
/// paragraph text. Every third sentence is a claim and every fifth a link,
/// so both edit targets appear throughout the document.
pub fn draft(rng: &mut Rng, body_target: usize) -> Draft {
    let heading = heading(rng);
    let mut paragraphs = Vec::new();
    let mut written = 0;
    let mut sentence_index = 0;
    while written < body_target {
        let count = rng.range(3, 8);
        let mut sentences = Vec::with_capacity(count);
        for _ in 0..count {
            let sentence = if sentence_index % 5 == 4 {
                link_sentence(rng)
            } else if sentence_index % 3 == 2 {
                claim_sentence(rng)
            } else {
                plain_sentence(rng)
            };
            sentence_index += 1;
            written += sentence.len() + 1;
            sentences.push(sentence);
        }
        paragraphs.push(sentences.join(" "));
    }
    Draft {
        heading,
        paragraphs,
    }
}

/// Renders one paragraph text into lines: hard-wrapped at [`WRAP_WIDTH`],
/// or one long line when `wrapped` is false.
fn render_paragraph(text: &str, wrapped: bool) -> Vec<String> {
    if !wrapped {
        return vec![text.to_string()];
    }
    wrap(text, WRAP_WIDTH)
}

/// Greedy word wrap at `width`. A word longer than the width stays on its
/// own line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The document body with one blank line between blocks: the heading,
/// then each rendered paragraph. Edit regions stay inside this body; the
/// exact-size tail follows it.
pub fn document_body(draft: &Draft, wrapped: bool) -> Vec<String> {
    let mut lines = vec![draft.heading.clone()];
    for paragraph in &draft.paragraphs {
        lines.push(String::new());
        lines.extend(render_paragraph(paragraph, wrapped));
    }
    lines
}

/// The single line of a mass-paragraph document: sentence text truncated
/// to exactly `bytes - 1` characters, so the file is one line plus its
/// final newline (SPEC 17.2 "an included 10 MiB paragraph").
pub fn mass_line(rng: &mut Rng, bytes: usize) -> String {
    assert!(bytes > 256, "a mass paragraph needs room for sentences");
    let target = bytes - 1;
    let mut text = String::new();
    while text.len() < target {
        if text.len() % 3 == 2 {
            text.push_str(&claim_sentence(rng));
        } else if text.len() % 5 == 4 {
            text.push_str(&link_sentence(rng));
        } else {
            text.push_str(&plain_sentence(rng));
        }
        text.push(' ');
    }
    text.truncate(target);
    text
}

/// The deterministic filler tail that brings a document to its exact byte
/// total. Edits change the body length; the tail absorbs the difference, so
/// every version of a document keeps the exact same size.
pub struct Tail;

impl Tail {
    /// Renders the tail so that `body.len() + tail.len() == target`.
    ///
    /// The tail is one filler line: enough repetitions of the filler
    /// sentence truncated to the remaining width. A body that already
    /// exceeds its budget fails generation instead of silently shrinking a
    /// corpus document.
    pub fn render(body_len: usize, target: usize) -> String {
        assert!(
            body_len + 2 <= target,
            "the document body ({body_len} bytes) leaves no tail room in the {target}-byte budget"
        );
        let width = target - body_len - 1;
        let filler = "Bench filler text keeps every document at its exact recorded size. ";
        let mut tail = String::new();
        while tail.len() < width {
            tail.push_str(filler);
        }
        tail.truncate(width);
        tail.push('\n');
        tail
    }
}

/// Repeated-line filler for the pathological repeated-line documents: one
/// sentence repeated to the exact byte total, with every line the same
/// length except a final padded remainder line.
pub fn repeated_lines(seed_text: &str, target: usize) -> Vec<String> {
    assert!(
        target >= seed_text.len() * 2 + 2,
        "no room for repeated lines"
    );
    let mut lines = Vec::new();
    let mut written = 0;
    while written + seed_text.len() < target - 1 {
        lines.push(seed_text.to_string());
        written += seed_text.len() + 1;
    }
    let remaining = target - 1 - written;
    if remaining > 0 {
        let mut last = String::new();
        while last.len() < remaining {
            last.push_str(seed_text);
            last.push(' ');
        }
        last.truncate(remaining);
        lines.push(last);
    }
    assert_eq!(
        lines.iter().map(|line| line.len() + 1).sum::<usize>(),
        target,
        "repeated lines must fill the exact target"
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_the_width() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        for line in wrap(text, 20) {
            assert!(line.len() <= 20, "line too long: {line}");
        }
    }

    #[test]
    fn tail_hits_the_exact_total() {
        for (body, target) in [(10, 64), (0, 32), (100, 4096)] {
            let tail = Tail::render(body, target);
            assert_eq!(body + tail.len(), target);
        }
    }

    #[test]
    fn repeated_lines_hit_the_exact_total() {
        for target in [512, 1024, 4096] {
            let lines = repeated_lines("The repeated line stays identical. ", target);
            assert_eq!(
                lines.iter().map(|line| line.len() + 1).sum::<usize>(),
                target
            );
        }
    }

    #[test]
    fn swap_pairs_keep_their_lengths() {
        for (left, right) in SWAP_PAIRS {
            assert_eq!(left.len(), right.len());
        }
    }
}
