#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use llmenv::config::Config;
use llmenv::scope;
use std::fs;
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
    tags: [rust, cli]
  - name: web-dev
    tags: [typescript, react]
  - name: mobile-dev
    tags: [swift, ios]

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

criterion_group!(
    benches,
    benchmark_config_parsing,
    benchmark_scope_evaluation,
);
criterion_main!(benches);
