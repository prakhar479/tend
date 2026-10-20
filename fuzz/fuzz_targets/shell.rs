//! The shell reader and everything built on it never panic, and prefix lengths are
//! valid slice points (I5, T5).
#![no_main]

use libfuzzer_sys::fuzz_target;
use tend_core::perception::{classify, command_key, shell, test_write_targets, writes_files};

fuzz_target!(|data: &[u8]| {
    let Ok(cmd) = std::str::from_utf8(data) else {
        return;
    };
    let _ = shell::parse(cmd);
    let (prefix, _) = shell::leading_cd(cmd);
    assert!(cmd.is_char_boundary(prefix));
    let _ = &cmd[prefix..];
    let _ = command_key(cmd);
    let _ = test_write_targets(cmd);
    let _ = writes_files(cmd);
    let _ = classify::classify(cmd);
});
