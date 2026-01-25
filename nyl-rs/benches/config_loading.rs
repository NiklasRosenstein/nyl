use criterion::{black_box, criterion_group, criterion_main, Criterion};
use nyl::config::ProjectConfig;
use std::fs;
use tempfile::TempDir;

fn bench_config_discovery(c: &mut Criterion) {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("nyl-project.yaml");
    fs::write(
        &config_path,
        r#"
settings:
  components_path: ./components
  search_path:
    - ./charts
    - ./manifests
"#,
    )
    .unwrap();

    c.bench_function("config_discovery", |b| {
        b.iter(|| {
            let _result = ProjectConfig::find(Some(black_box(temp.path())));
        });
    });
}

fn bench_config_parsing(c: &mut Criterion) {
    let yaml_content = r#"
settings:
  components_path: ./components
  search_path:
    - ./charts
    - ./manifests
  default_namespace: default

profiles:
  dev:
    values:
      environment: development
      debug: true
      replicas: 1
  prod:
    values:
      environment: production
      debug: false
      replicas: 3
"#;

    c.bench_function("config_parsing_simple", |b| {
        b.iter(|| {
            let _result: Result<serde_json::Value, _> =
                serde_norway::from_str(black_box(yaml_content));
        });
    });
}

fn bench_config_load_full(c: &mut Criterion) {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("nyl-project.yaml");
    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    fs::write(
        &config_path,
        r#"
settings:
  components_path: ./components
  search_path:
    - ./charts
  default_namespace: default
"#,
    )
    .unwrap();

    c.bench_function("config_load_full", |b| {
        b.iter(|| {
            let _result = ProjectConfig::load(Some(black_box(config_path.clone())));
        });
    });
}

criterion_group!(
    benches,
    bench_config_discovery,
    bench_config_parsing,
    bench_config_load_full,
);
criterion_main!(benches);
