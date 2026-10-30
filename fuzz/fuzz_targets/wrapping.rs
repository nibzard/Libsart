#![no_main]

use libfuzzer_sys::fuzz_target;
use margin::tui::wrap::{wrap_document, wrap_pieces};

fn without_spaces(text: &str) -> String {
    text.chars().filter(|&c| c != ' ').collect()
}

fuzz_target!(|data: &[u8]| {
    let Some((&width, bytes)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    // Include zero and one: these exercise the minimum-width behavior.
    let width = u16::from(width);
    for line in text.lines() {
        let pieces = wrap_pieces(line, usize::from(width));
        assert!(!pieces.is_empty());
        // Wrapping may remove ASCII spaces, but no other source characters.
        assert_eq!(without_spaces(&pieces.concat()), without_spaces(line));
    }
    let rows = wrap_document(text, width);
    let lines: Vec<_> = text.lines().collect();
    assert!(!rows.is_empty());
    let mut previous_line = 1;
    let mut reconstructed = vec![String::new(); lines.len().max(1)];
    for row in rows {
        assert!(row.source_line >= previous_line);
        assert!(row.source_line >= 1);
        assert!(row.source_line as usize <= reconstructed.len());
        reconstructed[row.source_line as usize - 1].push_str(&row.text);
        previous_line = row.source_line;
    }
    for (source, wrapped) in lines.iter().zip(&reconstructed) {
        assert_eq!(without_spaces(source), without_spaces(wrapped));
    }
});
