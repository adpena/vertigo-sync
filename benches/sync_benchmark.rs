use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::fs;
use tempfile::TempDir;

/// Create a realistic test project with the Vertigo directory structure.
/// Generates `file_count` Luau modules spread across Server, Client, and Shared.
fn create_test_project(file_count: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(src.join("Server/Services")).unwrap();
    fs::create_dir_all(src.join("Server/World/Builders")).unwrap();
    fs::create_dir_all(src.join("Client/Controllers")).unwrap();
    fs::create_dir_all(src.join("Client/UI")).unwrap();
    fs::create_dir_all(src.join("Shared/Config")).unwrap();
    fs::create_dir_all(src.join("Shared/Util")).unwrap();
    fs::create_dir_all(src.join("Shared/Net")).unwrap();

    let subdirs = [
        "Server/Services",
        "Server/World/Builders",
        "Client/Controllers",
        "Client/UI",
        "Shared/Config",
        "Shared/Util",
        "Shared/Net",
    ];

    for i in 0..file_count {
        let subdir = subdirs[i % subdirs.len()];
        let name = format!("Module{i}.luau");
        // Realistic Luau content with --!strict, service/controller pattern,
        // and enough body to approximate real file sizes (~500-800 bytes).
        let content = format!(
            concat!(
                "--!strict\n",
                "local ReplicatedStorage = game:GetService(\"ReplicatedStorage\")\n",
                "local Shared = ReplicatedStorage:WaitForChild(\"Shared\")\n",
                "\n",
                "local Module{i} = {{}}\n",
                "\n",
                "function Module{i}:Init()\n",
                "\tself._data = {{}}\n",
                "\tself._connections = {{}}\n",
                "\tfor idx = 1, 10 do\n",
                "\t\tself._data[idx] = idx * {i}\n",
                "\tend\n",
                "end\n",
                "\n",
                "function Module{i}:Start()\n",
                "\tfor _, v in self._data do\n",
                "\t\ttable.insert(self._connections, v)\n",
                "\tend\n",
                "end\n",
                "\n",
                "return Module{i}\n",
            ),
            i = i
        );
        fs::write(src.join(subdir).join(&name), &content).unwrap();
    }

    dir
}

/// Mutate a single file inside the test project to simulate an incremental edit.
fn touch_file(dir: &TempDir, index: usize) {
    let subdirs = [
        "Server/Services",
        "Server/World/Builders",
        "Client/Controllers",
        "Client/UI",
        "Shared/Config",
        "Shared/Util",
        "Shared/Net",
    ];
    let subdir = subdirs[index % subdirs.len()];
    let path = dir
        .path()
        .join("src")
        .join(subdir)
        .join(format!("Module{index}.luau"));
    let mut content = fs::read_to_string(&path).unwrap();
    content.push_str(&format!("\n-- modified {index}\n"));
    fs::write(&path, &content).unwrap();
}

// ---------------------------------------------------------------------------
// Benchmark 1: Cold snapshot (no cache, full SHA-256 of all files)
// ---------------------------------------------------------------------------
fn bench_snapshot_cold(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];

    c.bench_function("snapshot_cold_529_files", |b| {
        b.iter(|| {
            black_box(vertigo_sync::build_snapshot(dir.path(), &includes).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 2: Cached snapshot, 0 changes (pure mtime/size check, no hashing)
// ---------------------------------------------------------------------------
fn bench_snapshot_cached_no_changes(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];
    let mut cache = vertigo_sync::SnapshotCache::new();

    // Warm the cache with an initial build.
    vertigo_sync::build_snapshot_cached(dir.path(), &includes, &mut cache).unwrap();

    c.bench_function("snapshot_cached_529_files_0_changes", |b| {
        b.iter(|| {
            black_box(
                vertigo_sync::build_snapshot_cached(dir.path(), &includes, &mut cache).unwrap(),
            );
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 3: Cached snapshot, 1 file changed (1 hash + 528 cache hits)
// ---------------------------------------------------------------------------
fn bench_snapshot_cached_1_change(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];
    let mut cache = vertigo_sync::SnapshotCache::new();

    // Warm cache.
    vertigo_sync::build_snapshot_cached(dir.path(), &includes, &mut cache).unwrap();

    c.bench_function("snapshot_cached_529_files_1_change", |b| {
        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            for i in 0..iters {
                // Touch a different file each iteration to avoid OS-level page caching bias.
                touch_file(&dir, (i as usize) % 529);
                black_box(
                    vertigo_sync::build_snapshot_cached(dir.path(), &includes, &mut cache).unwrap(),
                );
            }
            start.elapsed()
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 4: Cached snapshot, 10 files changed
// ---------------------------------------------------------------------------
fn bench_snapshot_cached_10_changes(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];
    let mut cache = vertigo_sync::SnapshotCache::new();

    // Warm cache.
    vertigo_sync::build_snapshot_cached(dir.path(), &includes, &mut cache).unwrap();

    c.bench_function("snapshot_cached_529_files_10_changes", |b| {
        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            for i in 0..iters {
                let base = (i as usize * 10) % 529;
                for offset in 0..10 {
                    touch_file(&dir, (base + offset) % 529);
                }
                black_box(
                    vertigo_sync::build_snapshot_cached(dir.path(), &includes, &mut cache).unwrap(),
                );
            }
            start.elapsed()
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 5: Diff computation between two snapshots
// ---------------------------------------------------------------------------
fn bench_diff_computation(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];

    let snap_a = vertigo_sync::build_snapshot(dir.path(), &includes).unwrap();

    // Modify 20 files to create meaningful diffs.
    for i in 0..20 {
        touch_file(&dir, i * 26);
    }
    let snap_b = vertigo_sync::build_snapshot(dir.path(), &includes).unwrap();

    c.bench_function("diff_snapshots_529_files_20_modified", |b| {
        b.iter(|| {
            black_box(vertigo_sync::diff_snapshots(&snap_a, &snap_b));
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 6: Snapshot serialization to JSON
// ---------------------------------------------------------------------------
fn bench_snapshot_serialize(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];
    let snapshot = vertigo_sync::build_snapshot(dir.path(), &includes).unwrap();

    c.bench_function("snapshot_serialize_json_529_entries", |b| {
        b.iter(|| {
            black_box(serde_json::to_string(&snapshot).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 7: Health doctor (full validation pass)
// ---------------------------------------------------------------------------
fn bench_health_doctor(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];

    // Create a minimal default.project.json so doctor doesn't warn about missing it.
    fs::write(
        dir.path().join("default.project.json"),
        r#"{"name":"bench","tree":{"$path":"src/"}}"#,
    )
    .unwrap();

    c.bench_function("health_doctor_529_files", |b| {
        b.iter(|| {
            black_box(vertigo_sync::run_health_doctor(dir.path(), &includes).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 8: Source validation (Luau lint rules)
// ---------------------------------------------------------------------------
fn bench_validation(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];

    c.bench_function("validate_source_529_files", |b| {
        b.iter(|| {
            black_box(vertigo_sync::validate::validate_source(dir.path(), &includes).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Scaling benchmarks: measure how snapshot time grows with project size
// ---------------------------------------------------------------------------
fn bench_snapshot_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_scaling");
    for &count in &[100, 250, 529, 1000] {
        let dir = create_test_project(count);
        let includes = vec!["src".to_string()];
        group.bench_function(format!("cold_{count}_files"), |b| {
            b.iter(|| {
                black_box(vertigo_sync::build_snapshot(dir.path(), &includes).unwrap());
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 9: Snapshot with mixed file types (json, txt, csv)
// ---------------------------------------------------------------------------
fn create_mixed_file_project(file_count: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(src.join("Server")).unwrap();
    fs::create_dir_all(src.join("Shared/Config")).unwrap();

    for i in 0..file_count {
        match i % 5 {
            0 => {
                fs::write(
                    src.join(format!("Server/Mod{i}.luau")),
                    format!("--!strict\nreturn {i}\n"),
                )
                .unwrap();
            }
            1 => {
                fs::write(
                    src.join(format!("Shared/Config/cfg{i}.json")),
                    format!(r#"{{"key":{i}}}"#),
                )
                .unwrap();
            }
            2 => {
                fs::write(
                    src.join(format!("Shared/note{i}.txt")),
                    format!("note content {i}"),
                )
                .unwrap();
            }
            3 => {
                fs::write(
                    src.join(format!("Shared/locale{i}.csv")),
                    format!("key,en\nhello{i},Hello"),
                )
                .unwrap();
            }
            _ => {
                fs::write(
                    src.join(format!("Server/Mod{i}.luau")),
                    format!("--!strict\nreturn {i}\n"),
                )
                .unwrap();
            }
        }
    }
    dir
}

fn bench_snapshot_cold_mixed(c: &mut Criterion) {
    let dir = create_mixed_file_project(529);
    let includes = vec!["src".to_string()];

    c.bench_function("snapshot_cold_529_mixed_files", |b| {
        b.iter(|| {
            black_box(vertigo_sync::build_snapshot(dir.path(), &includes).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 10: Snapshot with .meta.json sidecars
// ---------------------------------------------------------------------------
fn bench_snapshot_with_meta_json(c: &mut Criterion) {
    let dir = create_test_project(200);
    let src = dir.path().join("src");

    // Add .meta.json sidecars for some files.
    for i in (0..200).step_by(5) {
        let subdirs = [
            "Server/Services",
            "Server/World/Builders",
            "Client/Controllers",
            "Client/UI",
            "Shared/Config",
            "Shared/Util",
            "Shared/Net",
        ];
        let subdir = subdirs[i % subdirs.len()];
        let meta_path = src.join(subdir).join(format!("Module{i}.meta.json"));
        fs::write(&meta_path, r#"{"properties":{"Disabled":false}}"#).unwrap();
    }

    let includes = vec!["src".to_string()];
    c.bench_function("snapshot_cold_200_files_with_meta_json", |b| {
        b.iter(|| {
            black_box(vertigo_sync::build_snapshot(dir.path(), &includes).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 11: History reading
// ---------------------------------------------------------------------------
fn bench_history_read(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");

    // Write 256 events.
    let mut content = String::new();
    for i in 1..=256 {
        content.push_str(&format!(
            r#"{{"seq":{},"snapshot_hash":"hash_{:04x}","timestamp_utc":"2026-01-01T00:00:{}Z","diff":{{"added":1,"modified":0,"deleted":0}}}}"#,
            i, i, i
        ));
        content.push('\n');
    }
    fs::write(&log_path, &content).unwrap();

    c.bench_function("history_read_256_entries", |b| {
        b.iter(|| {
            black_box(vertigo_sync::read_history(&log_path, 256).unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 12: Reverse diff computation
// ---------------------------------------------------------------------------
fn bench_reverse_diff(c: &mut Criterion) {
    let dir = create_test_project(529);
    let includes = vec!["src".to_string()];

    let snap_a = vertigo_sync::build_snapshot(dir.path(), &includes).unwrap();
    for i in 0..20 {
        touch_file(&dir, i * 26);
    }
    let snap_b = vertigo_sync::build_snapshot(dir.path(), &includes).unwrap();
    let diff = vertigo_sync::diff_snapshots(&snap_a, &snap_b);

    c.bench_function("reverse_diff_computation", |b| {
        b.iter(|| {
            black_box(vertigo_sync::reverse_diff(&diff));
        });
    });
}

criterion_group!(
    benches,
    bench_snapshot_cold,
    bench_snapshot_cached_no_changes,
    bench_snapshot_cached_1_change,
    bench_snapshot_cached_10_changes,
    bench_diff_computation,
    bench_snapshot_serialize,
    bench_health_doctor,
    bench_validation,
    bench_snapshot_scaling,
    bench_snapshot_cold_mixed,
    bench_snapshot_with_meta_json,
    bench_history_read,
    bench_reverse_diff,
);
criterion_main!(benches);
