//! Failure extraction, normalization, and shaping never panic, and a summary never
//! has more lines than the output (I5, T5).
#![no_main]

use libfuzzer_sys::fuzz_target;
use tend_core::perception::{extract_failures, normalize_message};
use tend_core::shape::shape;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = tend_core::text::redact(&text);
    let _ = tend_core::text::strip_ansi(&text);
    for f in extract_failures(&text) {
        let _ = normalize_message(&f.message);
    }
    for level in 1..=3 {
        if let Some(s) = shape(&text, level, 1.0) {
            assert!(s.to_lines <= s.from_lines);
        }
    }
});
