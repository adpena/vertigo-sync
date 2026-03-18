#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = vertigo_sync::rbxl::RbxlLoader::load_xml_str(text);
    }
});
