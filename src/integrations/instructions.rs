//! Safe ownership checks for the optional agent instruction block.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::claude::SetupAction;
use crate::app::OperationError;

pub const LEGACY_TEMPLATE: &str = "<!-- margin:notes v1 -->\nAfter you checkpoint a draft, explain each meaningful change with Margin:\n`margin note --group <digest> --text \"<why>\"` — group digests come from\n`margin diff --json`. Writers read these notes beside your edits and\nanswer with their own; `margin notes --json` lists theirs. Native Git\nequivalent: `git notes --ref=margin`; `margin notes --schema` prints\nthe envelope schema. IDs must also be unique within each commit's notes.\n<!-- /margin:notes -->";
pub const TEMPLATE: &str = "<!-- margin:notes v1 -->\nAfter you checkpoint a draft, explain each meaningful change with Margin:\n`margin note --group <digest> --text \"<why>\"` — group digests come from\n`margin diff --json`. Writers read these notes beside your edits and\nanswer with their own; `margin notes --json` lists theirs. Native Git\nequivalent: `git notes --ref=margin`; `margin notes --schema` prints\nthe envelope schema. IDs must also be unique within each commit's notes.\n\nFor a queued edit request, use `margin requests list --json`, then\n`margin requests inspect ID --json` and `margin requests claim ID --owner SESSION --json`.\nRead the captured source and replace only its start..end byte span.\nWrite the replacement to a temporary UTF-8 file outside the writing scope, then use\n`margin requests submit ID --owner SESSION --input FILE --explanation TEXT --json`.\nMargin applies results after saving pending input and checking the captured passage.\nKeep edits inside the requested span. Respect cancelled and undone requests.\nDo not write the requested document directly. A request never approves edits.\n<!-- /margin:notes -->";
pub const SKILL_PATH: &str = ".claude/skills/margin/SKILL.md";
pub const SKILL_TEMPLATE: &str = include_str!("margin-skill.md");
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct Patch {
    pub path: PathBuf,
    pub planned_text: String,
    pub changed: bool,
    #[serde(skip)]
    pub expected: Option<Vec<u8>>,
}

fn conflict(path: &Path, reason: &str) -> OperationError {
    OperationError::new(
        "INSTRUCTION_CONFLICT",
        format!("{}: {reason}; instruction file preserved", path.display()),
    )
}

pub fn read(path: &Path) -> Result<Option<Vec<u8>>, OperationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(conflict(path, "instruction target is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(conflict(path, &format!("cannot inspect file: {error}"))),
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| conflict(path, &format!("cannot read file: {error}")))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(conflict(path, "instruction file exceeds 1 MiB"));
    }
    Ok(Some(bytes))
}

pub fn plan(root: &Path, action: SetupAction) -> Result<Vec<Patch>, OperationError> {
    ["CLAUDE.md", "AGENTS.md", SKILL_PATH]
        .into_iter()
        .map(|name| {
            let path = root.join(name);
            let expected = read(&path)?;
            let text = std::str::from_utf8(expected.as_deref().unwrap_or_default())
                .map_err(|_| conflict(&path, "instruction file is not UTF-8"))?;
            let planned_text = if name == SKILL_PATH {
                let previous_skill = SKILL_TEMPLATE.replace(
                    "Use `margin context --json` for writing state, viewer context, and pending requests.\nViewer context is the last published snapshot. Check `captured_ms` before using it.\nSelection offsets count UTF-8 bytes in the active document identified by `digest`.\nPreviews can be truncated. Inspect the request for complete captured source.\nA missing viewer means no desk is attached; do not infer a selection.\nUse `margin requests list --json` for the complete request list.",
                    "Use `margin status --json` for writing state and `margin requests list --json` for requests.");
                if !text.is_empty() && text != SKILL_TEMPLATE && text != previous_skill {
                    return Err(conflict(
                        &path,
                        "skill was edited or belongs to another tool",
                    ));
                }
                match action {
                    SetupAction::Install => SKILL_TEMPLATE.to_string(),
                    SetupAction::Remove => String::new(),
                }
            } else {
                transform(&path, text, action)?
            };
            let changed = planned_text.as_bytes() != expected.as_deref().unwrap_or_default();
            Ok(Patch {
                path,
                planned_text,
                changed,
                expected,
            })
        })
        .collect()
}

pub fn verify(patch: &Patch) -> Result<(), OperationError> {
    if read(&patch.path)? != patch.expected {
        return Err(OperationError::new(
            "STALE_STATE",
            format!(
                "{} changed after planning; this instruction file is preserved",
                patch.path.display()
            ),
        )
        .retryable());
    }
    Ok(())
}

fn transform(path: &Path, text: &str, action: SetupAction) -> Result<String, OperationError> {
    let openings: Vec<_> = text.match_indices("<!-- margin:notes").collect();
    let closings: Vec<_> = text.match_indices("<!-- /margin:notes").collect();
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let template = TEMPLATE.replace('\n', newline);
    if openings.is_empty() && closings.is_empty() {
        return Ok(match action {
            SetupAction::Remove => text.to_string(),
            SetupAction::Install => format!(
                "{text}{}{template}{newline}",
                if text.is_empty() || text.ends_with('\n') {
                    ""
                } else {
                    newline
                }
            ),
        });
    }
    if openings.len() != 1 || closings.len() != 1 {
        return Err(conflict(
            path,
            "duplicate, nested, or unmatched notes markers",
        ));
    }
    let start = openings[0].0;
    let closing = closings[0].0;
    let end = closing + "<!-- /margin:notes -->".len();
    if start >= closing
        || end > text.len()
        || (text.get(start..end) != Some(template.as_str())
            && text.get(start..end) != Some(LEGACY_TEMPLATE.replace('\n', newline).as_str()))
    {
        return Err(conflict(
            path,
            "notes block was edited, malformed, or declares an unknown version",
        ));
    }
    Ok(match action {
        SetupAction::Install => format!("{}{}{}", &text[..start], template, &text[end..]),
        SetupAction::Remove => format!("{}{}", &text[..start], &text[end..]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skill_install_remove_and_conflict() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SKILL_PATH);
        let patches = plan(root.path(), SetupAction::Install).unwrap();
        assert_eq!(patches[2].planned_text, SKILL_TEMPLATE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, SKILL_TEMPLATE).unwrap();
        assert!(!plan(root.path(), SetupAction::Install).unwrap()[2].changed);
        assert!(
            plan(root.path(), SetupAction::Remove).unwrap()[2]
                .planned_text
                .is_empty()
        );
        fs::write(&path, "User skill").unwrap();
        assert!(plan(root.path(), SetupAction::Install).is_err());
        assert!(plan(root.path(), SetupAction::Remove).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "User skill");
    }

    #[test]
    fn reread_refuses_concurrent_edits_and_file_creation() {
        let root = tempfile::tempdir().unwrap();
        let patches = plan(root.path(), SetupAction::Install).unwrap();
        fs::write(root.path().join("AGENTS.md"), "New user rules").unwrap();
        assert_eq!(verify(&patches[1]).unwrap_err().code, "STALE_STATE");
        let patches = plan(root.path(), SetupAction::Install).unwrap();
        fs::write(root.path().join("AGENTS.md"), "Changed user rules").unwrap();
        assert_eq!(verify(&patches[1]).unwrap_err().code, "STALE_STATE");
    }
    #[test]
    fn unchanged_legacy_guidance_upgrades_without_replacing_user_text() {
        let path = Path::new("AGENTS.md");
        let original = format!("My instructions\n{LEGACY_TEMPLATE}\nMore rules\n");
        let updated = transform(path, &original, SetupAction::Install).unwrap();
        assert_eq!(
            updated,
            format!("My instructions\n{TEMPLATE}\nMore rules\n")
        );
        assert_eq!(
            transform(path, &updated, SetupAction::Install).unwrap(),
            updated
        );
        assert!(
            transform(
                path,
                &original.replace("After you checkpoint", "After I checkpoint"),
                SetupAction::Install
            )
            .is_err()
        );
    }
    #[test]
    fn ownership_requires_exact_template_and_preserves_surroundings() {
        let path = Path::new("AGENTS.md");
        for newline in ["\n", "\r\n"] {
            let template = TEMPLATE.replace('\n', newline);
            let text = format!("User rules{newline}{template}{newline}More rules{newline}");
            assert_eq!(transform(path, &text, SetupAction::Install).unwrap(), text);
            assert_eq!(
                transform(path, &text, SetupAction::Remove).unwrap(),
                format!("User rules{newline}{newline}More rules{newline}")
            );
            for damaged in [
                text.replace("each meaningful", "every single"),
                text.replace("notes v1", "notes v99"),
                format!("{text}{template}"),
                text.replace("<!-- /margin:notes -->", ""),
                text.replace("<!-- margin:notes v1 -->", ""),
            ] {
                assert!(transform(path, &damaged, SetupAction::Install).is_err());
                assert!(transform(path, &damaged, SetupAction::Remove).is_err());
            }
        }
    }
}
