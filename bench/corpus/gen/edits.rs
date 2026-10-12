//! The edit engine of the benchmark corpora (SPEC 17.2).
//!
//! SPEC 17.2 asks generated corpora to include sparse and dense changes,
//! pure reflow, and changes to claims and link targets. Each kind here is
//! one complete, reviewable source change:
//!
//! - [`EditKind::Word`] swaps an equal-length vocabulary pair in one line
//!   (a sparse change; byte length is preserved).
//! - [`EditKind::Claim`] advances the single-digit counts of one claim.
//! - [`EditKind::LinkTarget`] advances a link target while its label and
//!   surrounding text stay identical.
//! - [`EditKind::Reflow`] rewraps one paragraph — unwrap to a single line
//!   or wrap at [`text::WRAP_WIDTH`] — without changing one word.
//! - [`EditKind::Dense`] replaces one whole paragraph with fresh prose of
//!   the same exact byte size, rendered as a single line.
//!
//! Every edit is placed at least [`MIN_SEPARATION`] lines away from the
//! previous one, so the fixed diff contract of SPEC 10.1 (`--unified=3`,
//! `--inter-hunk-context=0`) emits one independent hunk per edit and the
//! manifest's expected hunk counts are exact.

use super::rng::Rng;
use super::text::{SWAP_PAIRS, WRAP_WIDTH, wrap};

/// Unchanged lines required between two edit regions. Under the fixed
/// contract (`--unified=3`, `--inter-hunk-context=0`) two changed lines six
/// lines apart or closer merge into one hunk, so eight unchanged lines
/// keep every edit its own hunk with room to spare.
const MIN_SEPARATION: usize = 8;

/// The edit kinds the corpora use, in manifest order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditKind {
    /// One equal-length word swap in one line.
    Word,
    /// One claim count advanced, same line length.
    Claim,
    /// One link target advanced, label unchanged.
    LinkTarget,
    /// One paragraph unwrapped or rewrapped, words unchanged.
    Reflow,
    /// One paragraph replaced with fresh prose.
    Dense,
}

impl EditKind {
    /// The manifest class label.
    pub fn label(self) -> &'static str {
        match self {
            EditKind::Word => "sparse-word",
            EditKind::Claim => "claim-value",
            EditKind::LinkTarget => "link-target",
            EditKind::Reflow => "pure-reflow",
            EditKind::Dense => "dense-rewrite",
        }
    }
}

/// The kinds a document of one rendering admits. Single-line documents —
/// the mass paragraph and the repeated-line cases — take only the
/// length-preserving single-line edits, so their pathological shape
/// survives the change.
pub fn single_line_kinds() -> Vec<EditKind> {
    vec![EditKind::Word, EditKind::Claim, EditKind::LinkTarget]
}

/// The kinds an ordinary prose document admits. The round-robin order
/// decides which kind each planned edit tries first, so wrapped prose —
/// whose reflow direction is unwrapping — reaches `Reflow` one edit
/// sooner than long-line prose, whose reflow wraps.
pub fn prose_kinds(wrapped: bool) -> Vec<EditKind> {
    let mut kinds = vec![
        EditKind::Word,
        EditKind::Claim,
        EditKind::LinkTarget,
        EditKind::Dense,
    ];
    if wrapped {
        kinds.insert(3, EditKind::Reflow);
    } else {
        kinds.push(EditKind::Reflow);
    }
    kinds
}

/// One paragraph of the working document: consecutive non-blank lines that
/// are not the heading block.
struct Paragraph {
    start: usize,
    end: usize, // exclusive
    lines: usize,
}

/// Finds the paragraphs of `lines`, skipping the leading heading block.
fn paragraphs(lines: &[String]) -> Vec<Paragraph> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && !lines[index].trim().is_empty() {
            index += 1;
        }
        let is_heading = lines[start].starts_with("# ");
        if !is_heading {
            found.push(Paragraph {
                start,
                end: index,
                lines: index - start,
            });
        }
    }
    found
}

/// Advances the two single-digit counts of one claim line in place.
/// Returns false when the line carries no claim.
fn bump_claim(line: &mut String) -> bool {
    for prefix in ["records ", "count "] {
        let Some(at) = line.find(prefix) else {
            continue;
        };
        let digits = at + prefix.len();
        let bytes = line.as_bytes();
        // "D of D": digit, " of ", digit.
        if bytes.len() < digits + 6 {
            continue;
        }
        if bytes[digits].is_ascii_digit()
            && &line[digits + 1..digits + 5] == " of "
            && bytes[digits + 5].is_ascii_digit()
        {
            let left = (bytes[digits] - b'0' + 1) % 10;
            let right = (bytes[digits + 5] - b'0' + 1) % 10;
            let mut updated = String::with_capacity(line.len());
            updated.push_str(&line[..digits]);
            updated.push((b'0' + left) as char);
            updated.push_str(" of ");
            updated.push((b'0' + right) as char);
            updated.push_str(&line[digits + 6..]);
            *line = updated;
            return true;
        }
    }
    false
}

/// Advances the note-site number of one generated link in place. Returns
/// false when the line carries no generated link target.
fn bump_link(line: &mut String) -> bool {
    let Some(at) = line.find("docs/note-") else {
        return false;
    };
    let digits = at + "docs/note-".len();
    let bytes = line.as_bytes();
    if bytes.len() < digits + 3
        || !bytes[digits..digits + 3]
            .iter()
            .all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let site: u16 = line[digits..digits + 3].parse().expect("three digits");
    let next = format!("{:03}", (site + 1) % 1000);
    let mut updated = String::with_capacity(line.len());
    updated.push_str(&line[..digits]);
    updated.push_str(&next);
    updated.push_str(&line[digits + 3..]);
    *line = updated;
    true
}

/// Swaps the first equal-length vocabulary pair found in one line. Returns
/// false when the line contains no pair member.
fn swap_word(line: &mut String) -> bool {
    for (left, right) in SWAP_PAIRS {
        if let Some(at) = line.find(left) {
            let boundary = |offset: usize, forward: bool| -> bool {
                let bytes = line.as_bytes();
                if forward {
                    offset >= bytes.len() || !bytes[offset].is_ascii_lowercase()
                } else {
                    offset == 0 || !bytes[offset - 1].is_ascii_lowercase()
                }
            };
            if boundary(at, false) && boundary(at + left.len(), true) {
                let mut updated = String::with_capacity(line.len());
                updated.push_str(&line[..at]);
                updated.push_str(right);
                updated.push_str(&line[at + left.len()..]);
                *line = updated;
                return true;
            }
        }
    }
    false
}

/// One region of the document an edit may touch. `end` is exclusive.
struct Region {
    start: usize,
    end: usize,
}

/// Says whether `region` stays far enough from every used region: it must
/// end well before one starts, or start well after one ends.
fn separated(region: &Region, used: &[Region]) -> bool {
    used.iter().all(|other| {
        region.end + MIN_SEPARATION <= other.start || other.end + MIN_SEPARATION <= region.start
    })
}

/// Applies `planned` edits to the body `lines`, using only `allowed`
/// kinds; an unavailable kind falls back to the next allowed one. Returns
/// the kinds actually applied, in order.
///
/// Edit `i` centers near line `(i + 1) * len / (planned + 1)`, so the
/// changes spread evenly and keep their separation without scanning the
/// whole document per edit.
pub fn apply(
    lines: &mut Vec<String>,
    rng: &mut Rng,
    planned: usize,
    allowed: &[EditKind],
) -> Vec<EditKind> {
    assert!(!allowed.is_empty(), "a document must admit one edit kind");
    let mut used: Vec<Region> = Vec::new();
    let mut applied = Vec::with_capacity(planned);
    for index in 0..planned {
        let center = (index + 1) * lines.len() / (planned + 1);
        let order: Vec<EditKind> = {
            let mut order = Vec::with_capacity(allowed.len());
            order.push(allowed[index % allowed.len()]);
            for kind in allowed {
                if !order.contains(kind) {
                    order.push(*kind);
                }
            }
            order
        };
        let mut done = false;
        for kind in order {
            if let Some(region) = apply_one(lines, rng, kind, center, &used) {
                used.push(region);
                applied.push(kind);
                done = true;
                break;
            }
        }
        assert!(
            done,
            "no applicable edit of {:?} remains near line {center}; the document is too small for {planned} separated edits",
            allowed
        );
    }
    applied
}

/// Tries one edit of `kind` near `center`. Returns the touched region when
/// it applies.
fn apply_one(
    lines: &mut Vec<String>,
    rng: &mut Rng,
    kind: EditKind,
    center: usize,
    used: &[Region],
) -> Option<Region> {
    let mut candidates: Vec<Paragraph> = paragraphs(lines);
    // Nearest paragraph to the center first.
    candidates.sort_by_key(|paragraph| paragraph.start.abs_diff(center));

    match kind {
        EditKind::Word | EditKind::Claim | EditKind::LinkTarget => {
            for paragraph in &candidates {
                for offset in 0..paragraph.lines {
                    for line_at in [paragraph.start + offset, paragraph.end - 1 - offset] {
                        if line_at >= lines.len() {
                            continue;
                        }
                        let region = Region {
                            start: line_at,
                            end: line_at + 1,
                        };
                        if !separated(&region, used) {
                            continue;
                        }
                        let changed = {
                            let line = &mut lines[line_at];
                            match kind {
                                EditKind::Word => swap_word(line),
                                EditKind::Claim => bump_claim(line),
                                _ => bump_link(line),
                            }
                        };
                        if changed {
                            return Some(region);
                        }
                    }
                }
            }
            None
        }
        EditKind::Reflow => {
            for paragraph in &candidates {
                let region = Region {
                    start: paragraph.start,
                    end: paragraph.end,
                };
                if !separated(&region, used) {
                    continue;
                }
                let text = lines[paragraph.start..paragraph.end].join(" ");
                let rendered = if paragraph.lines == 1 && text.len() > WRAP_WIDTH {
                    wrap(&text, WRAP_WIDTH)
                } else if paragraph.lines > 1 {
                    vec![text]
                } else {
                    continue;
                };
                if rendered.len() == paragraph.lines {
                    continue; // the other shape is already in place
                }
                let new_end = paragraph.start + rendered.len();
                lines.splice(paragraph.start..paragraph.end, rendered);
                return Some(Region {
                    start: paragraph.start,
                    end: new_end,
                });
            }
            None
        }
        EditKind::Dense => {
            for paragraph in &candidates {
                let region = Region {
                    start: paragraph.start,
                    end: paragraph.end,
                };
                if !separated(&region, used) {
                    continue;
                }
                // The replacement keeps the block's exact byte budget: the
                // paragraph becomes one fresh line of the same total size,
                // so the document's exact size survives a dense rewrite.
                let block_bytes: usize = lines[paragraph.start..paragraph.end]
                    .iter()
                    .map(|line| line.len() + 1)
                    .sum();
                if block_bytes < 96 {
                    continue;
                }
                let mut text = String::new();
                while text.len() < block_bytes - 1 {
                    let sentences = rng.range(3, 8);
                    text.push_str(&super::text::fresh_paragraph(rng, sentences));
                    text.push(' ');
                }
                text.truncate(block_bytes - 1);
                let new_end = paragraph.start + 1;
                lines.splice(paragraph.start..paragraph.end, [text]);
                return Some(Region {
                    start: paragraph.start,
                    end: new_end,
                });
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Vec<String> {
        let mut lines = vec!["# quick workbook".to_string()];
        for block in 0..20 {
            lines.push(format!(
                "The tally records 3 of 4 open items, and the quick guide trail shows field note {block}."
            ));
            lines.push(
                "See the [guide](https://example.invalid/docs/note-007) for the margin."
                    .to_string(),
            );
            lines.push(String::new());
        }
        lines
    }

    #[test]
    fn edits_land_apart_and_change_content() {
        let mut rng = Rng::seeded(7);
        let mut lines = body();
        let before = lines.clone();
        let applied = apply(&mut lines, &mut rng, 6, &prose_kinds(true));
        assert_eq!(applied.len(), 6);
        assert_ne!(before, lines);
    }

    #[test]
    fn claim_and_link_edits_keep_line_length() {
        let mut claim = String::from("The tally records 9 of 4 open items.");
        let length = claim.len();
        assert!(bump_claim(&mut claim));
        assert_eq!(claim.len(), length);
        assert_eq!(claim, "The tally records 0 of 5 open items.");

        let mut link =
            "See the [guide](https://example.invalid/docs/note-999) for the margin.".to_string();
        let length = link.len();
        assert!(bump_link(&mut link));
        assert_eq!(link.len(), length);
        assert!(link.contains("note-000"));
    }

    #[test]
    fn word_swap_requires_whole_words() {
        let mut line = String::from("The fieldwork scopes the trail.");
        assert!(!swap_word(&mut line));
        let mut line = String::from("The field scopes the trail.");
        assert!(swap_word(&mut line));
        assert_eq!(line, "The scope scopes the trail.");
    }
}
