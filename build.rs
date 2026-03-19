use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const PLUGIN_SOURCE_DIR: &str = "assets/plugin_src";
const PLUGIN_OUTPUT_PATH: &str = "assets/VertigoSyncPlugin.lua";
const GENERATED_HEADER: &str =
    "-- AUTO-GENERATED FILE. DO NOT EDIT DIRECTLY.\n-- Source modules: assets/plugin_src/*.lua\n\n";

fn main() {
    if let Err(err) = bundle_plugin() {
        panic!("failed to bundle Studio plugin: {err}");
    }
}

fn bundle_plugin() -> io::Result<()> {
    println!("cargo:rerun-if-changed={PLUGIN_SOURCE_DIR}");

    let mut source_files = collect_lua_files(Path::new(PLUGIN_SOURCE_DIR))?;
    source_files.sort();

    if source_files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no plugin source files found under assets/plugin_src",
        ));
    }

    for path in &source_files {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let mut bundled = String::with_capacity(GENERATED_HEADER.len() + source_files.len() * 1024);
    bundled.push_str(GENERATED_HEADER);

    for (index, path) in source_files.iter().enumerate() {
        let chunk = fs::read_to_string(path)?;
        bundled.push_str(&chunk);
        if !chunk.ends_with('\n') {
            bundled.push('\n');
        }
        if index + 1 != source_files.len() {
            bundled.push('\n');
        }
    }

    write_if_changed(Path::new(PLUGIN_OUTPUT_PATH), bundled)
}

fn collect_lua_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("lua")) && path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

fn write_if_changed(path: &Path, contents: String) -> io::Result<()> {
    match fs::read_to_string(path) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) | Err(_) => {}
    }

    fs::write(path, contents)
}
