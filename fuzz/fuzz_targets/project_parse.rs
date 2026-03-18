#![no_main]

use libfuzzer_sys::fuzz_target;
use std::fs;
use tempfile::tempdir;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("default.project.json");
        fs::write(&path, text).expect("write project");
        let _ = vertigo_sync::project::parse_project(&path);
    }
});
