#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{Criterion, criterion_group, criterion_main};
use llmenv::config::Config;
use llmenv::materialize::cache::hash_manifest;
use llmenv::merge::MergedManifest;
use llmenv::scope;
use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use tempfile::TempDir;

// Test fixture: minimal valid config YAML
const SMALL_CONFIG: &str = r#"
scope:
  network: []
  host: []
  user: []
  project: []

tag: {}
bundle: []
cache:
  sync_interval_minutes: 60
adapter:
  engine: claude-code
"#;

// Test fixture: realistic config with multiple scopes and tags
const LARGE_CONFIG: &str = r#"
scope:
  network:
    - id: lan
      match:
        cidr: 192.168.1.0/24
      tags: [internal, dev-network]
    - id: vpn
      match:
        cidr: 10.0.0.0/8
      tags: [secure, dev-network]
  host:
    - id: macbook
      match:
        hostname: macbook-pro
      tags: [macos, dev-host]
    - id: linux-workstation
      match:
        hostname: ubuntu-dev
      tags: [linux, dev-host]
    - id: desktop
      match:
        hostname: desktop-machine
      tags: [linux, dev-host]
  user:
    - id: eng
      match:
        user: alice
      tags: [engineering, staff]
    - id: contractor
      match:
        user: bob
      tags: [contractor]

tag:
  internal: ""
  dev-network: ""
  secure: ""
  macos: ""
  linux: ""
  dev-host: ""
  engineering: ""
  contractor: ""
  staff: ""
  rust: ""
  cli: ""
  typescript: ""
  react: ""
  swift: ""
  ios: ""

bundle:
  - name: rust-dev
    when: [rust, cli]
  - name: web-dev
    when: [typescript, react]
  - name: mobile-dev
    when: [swift, ios]

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#;

/// Write a config to a temp file and return its path for loading.
fn write_config_fixture(dir: &TempDir, yaml: &str) -> anyhow::Result<std::path::PathBuf> {
    let config_path = dir.path().join("llmenv.yaml");
    fs::write(&config_path, yaml)?;
    Ok(config_path)
}

fn benchmark_config_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("config_parsing");

    // Small config parsing
    group.bench_function("small_config", |b| {
        b.iter_with_setup(
            || {
                let dir = TempDir::new().unwrap();
                let path = write_config_fixture(&dir, SMALL_CONFIG).unwrap();
                (dir, path)
            },
            |(dir, path)| {
                let _ = black_box(Config::load(&path));
                drop(dir); // Keep dir alive for the duration
            },
        );
    });

    // Large config parsing
    group.bench_function("large_config", |b| {
        b.iter_with_setup(
            || {
                let dir = TempDir::new().unwrap();
                let path = write_config_fixture(&dir, LARGE_CONFIG).unwrap();
                (dir, path)
            },
            |(dir, path)| {
                let _ = black_box(Config::load(&path));
                drop(dir);
            },
        );
    });

    group.finish();
}

fn benchmark_scope_evaluation(c: &mut Criterion) {
    let mut group = c.benchmark_group("scope_evaluation");

    // Setup configs once
    let dir = TempDir::new().unwrap();
    let small_path = write_config_fixture(&dir, SMALL_CONFIG).unwrap();
    let large_path = write_config_fixture(&dir, LARGE_CONFIG).unwrap();

    let small_config = Config::load(&small_path).unwrap();
    let large_config = Config::load(&large_path).unwrap();

    // Scope evaluation on small config
    group.bench_function("small_config", |b| {
        b.iter(|| {
            let env = scope::matcher::Env::detect();
            let _ = black_box(scope::evaluate(&small_config, &env));
        });
    });

    // Scope evaluation on large config
    group.bench_function("large_config", |b| {
        b.iter(|| {
            let env = scope::matcher::Env::detect();
            let _ = black_box(scope::evaluate(&large_config, &env));
        });
    });

    group.finish();
}

/// Build a `MergedManifest` with `file_count` files of `bytes_per_file` bytes
/// each, written under `dir`, for profiling `hash_manifest`'s full-content-read
/// cost (#742).
fn manifest_with_files(dir: &TempDir, file_count: usize, bytes_per_file: usize) -> MergedManifest {
    let content = vec![b'x'; bytes_per_file];
    let mut files = BTreeMap::new();
    for i in 0..file_count {
        let rel = PathBuf::from(format!("file-{i}.txt"));
        let abs = dir.path().join(&rel);
        fs::write(&abs, &content).unwrap();
        files.insert(rel, abs);
    }
    MergedManifest {
        files,
        ..MergedManifest::default()
    }
}

/// Profile `cache::hash_manifest` (#742): it reads every file's *content* on
/// every `materialize` call, which fires on the `llmenv export` hot path
/// (invoked on every shell prompt via the hook). These groups establish where
/// wall-clock time starts to matter as bundle size (file count, total bytes)
/// grows, informing whether a cheaper mtime+size fast path would be worth the
/// cache-correctness risk it introduces (see `docs/design/hot-path-optimizations.md`).
fn benchmark_hash_manifest(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_manifest");

    // Realistic bundle: a handful of small config/skill files.
    let dir = TempDir::new().unwrap();
    let realistic = manifest_with_files(&dir, 20, 2_000);
    group.bench_function("realistic_20_files_2kb", |b| {
        b.iter(|| {
            let _ = black_box(hash_manifest(&realistic));
        });
    });

    // Many small files: stresses per-file syscall overhead over raw bytes hashed.
    let dir_many = TempDir::new().unwrap();
    let many_small = manifest_with_files(&dir_many, 2_000, 200);
    group.bench_function("stress_2000_files_200b", |b| {
        b.iter(|| {
            let _ = black_box(hash_manifest(&many_small));
        });
    });

    // Few large files: stresses raw bytes hashed over syscall count.
    let dir_large = TempDir::new().unwrap();
    let large_files = manifest_with_files(&dir_large, 5, 5_000_000);
    group.bench_function("stress_5_files_5mb", |b| {
        b.iter(|| {
            let _ = black_box(hash_manifest(&large_files));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_config_parsing,
    benchmark_scope_evaluation,
    benchmark_hash_manifest,
);
criterion_main!(benches);
