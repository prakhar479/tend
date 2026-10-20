//! The guard's code reader and rules never panic on any edit (I5, T5).
#![no_main]

use libfuzzer_sys::fuzz_target;
use tend_core::guard;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mid = (0..=text.len() / 2)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let (old, new) = text.split_at(mid);
    let _ = guard::code_only(&text);
    let _ = guard::judges("tests/test_a.py", old, new);
    let _ = guard::assess(old, new);
});
