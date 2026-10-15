use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub path: String,
    pub revision: Option<String>,
    pub fingerprint: String,
    pub captured_ms: u64,
    pub unsaved: bool,
    pub line: u32,
    pub drift: usize,
    pub source: bool,
    pub visible: bool,
}

use crate::app::{DocumentContent, Operations, ShowRequest};
use crate::tui::{search, shell, wrap};
use std::path::Path;

pub struct Reference {
    pub descriptor: Descriptor,
    pub text: Option<String>,
    pub unavailable: Option<String>,
    pub changed: bool,
    pub truncated: bool,
    pub rows: Vec<wrap::DisplayRow>,
    pub offset: usize,
    width: u16,
}
impl Reference {
    pub fn snapshot(
        operations: &Operations<'_>,
        path: &str,
        text: String,
        unsaved: bool,
        truncated: bool,
        line: u32,
        source: bool,
    ) -> Result<Self, String> {
        let fingerprint = operations
            .git()
            .hash_object(text.as_bytes())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            descriptor: Descriptor {
                path: path.into(),
                revision: None,
                fingerprint,
                captured_ms: super::session::now_ms(),
                unsaved,
                line,
                drift: 0,
                source,
                visible: true,
            },
            text: Some(text),
            unavailable: None,
            changed: false,
            truncated,
            rows: vec![],
            offset: 0,
            width: 0,
        })
    }
    pub fn working(
        operations: &Operations<'_>,
        root: &Path,
        path: &str,
        line: u32,
        unsaved: bool,
    ) -> Result<Self, String> {
        let (bytes, truncated) =
            search::read_text(root, path, 256 * 1024).map_err(|e| e.to_string())?;
        let text =
            search::valid_text(&bytes, truncated).ok_or("Reference is not readable UTF-8")?;
        Self::snapshot(
            operations,
            path,
            shell::prepare_display(text),
            unsaved,
            truncated,
            line,
            false,
        )
    }
    pub fn historical(
        operations: &Operations<'_>,
        path: &str,
        revision: &str,
        line: u32,
    ) -> Result<Self, String> {
        let shown = operations.show_document(&ShowRequest {
            path: path.into(),
            revision: revision.into(),
            max_bytes: Some(256 * 1024),
            max_lines: None,
            raw: false,
        });
        if let Some(e) = shown.error {
            return Err(e.message);
        }
        let shown = shown.data.ok_or("Reference revision unavailable")?;
        let (text, truncated) = match shown.content {
            DocumentContent::Missing => {
                return Err(
                    "Reference missing at selected revision (not an empty document)".into(),
                );
            }
            DocumentContent::Empty { .. } => (String::new(), false),
            DocumentContent::Text(t) => (shell::prepare_display(&t.text), t.truncated),
            _ => return Err("Reference is not readable UTF-8".into()),
        };
        let mut reference = Self::snapshot(operations, path, text, false, truncated, line, false)?;
        reference.descriptor.revision = Some(shown.revision_oid);
        Ok(reference)
    }
    pub fn resume(
        operations: &Operations<'_>,
        root: &Path,
        descriptor: Descriptor,
        listed: bool,
    ) -> Self {
        let result = if !listed {
            Err("Reference unavailable: outside the current scope".into())
        } else if let Some(oid) = &descriptor.revision {
            if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                Err("Reference unavailable: saved revision is not a full immutable OID".into())
            } else {
                Self::historical(operations, &descriptor.path, oid, descriptor.line)
            }
        } else {
            Self::working(
                operations,
                root,
                &descriptor.path,
                descriptor.line,
                descriptor.unsaved,
            )
            .and_then(|r| {
                if r.descriptor.fingerprint == descriptor.fingerprint {
                    Ok(r)
                } else {
                    Err("Reference unavailable: working source changed".into())
                }
            })
        };
        match result {
            Ok(mut r) => {
                r.descriptor = descriptor;
                r
            }
            Err(error) => Self {
                descriptor,
                text: None,
                unavailable: Some(if error.starts_with("Reference unavailable") {
                    error
                } else {
                    format!("Reference unavailable: {error}")
                }),
                changed: false,
                truncated: false,
                rows: vec![],
                offset: 0,
                width: 0,
            },
        }
    }
    pub fn label(&self) -> String {
        if let Some(oid) = &self.descriptor.revision {
            format!("Revision {}", super::render::short(oid))
        } else if self.descriptor.unsaved {
            "Unsaved working snapshot".into()
        } else {
            "Working snapshot".into()
        }
    }
    pub fn prepare(&mut self, width: u16) {
        if self.width != width || self.rows.is_empty() {
            self.rows = wrap::reading_document(
                self.text.as_deref().unwrap_or(""),
                width,
                self.descriptor.source,
            );
            let row = wrap::row_for_anchor(&self.rows, self.descriptor.line);
            self.offset = row
                + self.descriptor.drift.min(
                    wrap::rows_of_source(&self.rows, row, self.descriptor.line).saturating_sub(1),
                );
            self.width = width;
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(1));
        self.sync();
    }
    fn sync(&mut self) {
        if let Some(row) = self.rows.get(self.offset) {
            self.descriptor.line = row.source_line;
            self.descriptor.drift = self
                .offset
                .saturating_sub(wrap::row_for_anchor(&self.rows, row.source_line));
        }
    }
    pub fn scroll(&mut self, rows: i32) {
        self.offset = if rows >= 0 {
            self.offset.saturating_add(rows as usize)
        } else {
            self.offset.saturating_sub(rows.unsigned_abs() as usize)
        }
        .min(self.rows.len().saturating_sub(1));
        self.sync();
    }
    pub fn toggle_source(&mut self) {
        self.descriptor.source = !self.descriptor.source;
        self.width = 0;
    }
    pub fn search(&mut self, query: &str) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let found = (1..=self.rows.len())
            .map(|n| (self.offset + n) % self.rows.len())
            .find(|&i| self.rows[i].text.to_lowercase().contains(query));
        if let Some(row) = found {
            self.offset = row;
            self.sync();
            true
        } else {
            false
        }
    }
}
