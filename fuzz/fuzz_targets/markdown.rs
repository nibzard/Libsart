#![no_main]

use libfuzzer_sys::fuzz_target;
use margin::markdown::analyze;

fuzz_target!(|text: &str| {
    let document = analyze(text);
    // Check line offsets against an independent string-based oracle.
    let mut expected = vec![0];
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if line.ends_with('\n') {
            expected.push(offset);
        }
    }
    assert_eq!(document.line_starts, expected);
    let mut previous_start = 0;
    for block in &document.blocks {
        assert!(block.first_line >= 1);
        assert!(block.first_line >= previous_start);
        assert!(block.first_line <= block.last_line);
        assert!(block.last_line <= expected.len() as u64);
        previous_start = block.first_line;
    }
});
