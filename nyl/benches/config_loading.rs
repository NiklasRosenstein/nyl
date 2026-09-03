use criterion::{criterion_group, criterion_main, Criterion};
use nyl::config::ProjectConfig;
use std::fs;
use std::hint::black_box;
use tempfile::TempDir;

fn bench_config_discovery(c: &mut Criterion) {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("nyl.toml");
    fs::write(
        &config_path,
        r#"
[project]
components_search_paths = ["./components"]
helm_chart_search_paths = ["./charts", "./manifests"]
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
    let toml_content = r#"
[project]
components_search_paths = ["./components"]
helm_chart_search_paths = ["./charts", "./manifests"]
"#;

    c.bench_function("config_parsing_simple", |b| {
        b.iter(|| {
            let _result: Result<nyl::config::ProjectFile, _> = toml::from_str(black_box(toml_content));
        });
    });
}

fn bench_config_load_full(c: &mut Criterion) {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("nyl.toml");
    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    fs::write(
        &config_path,
        r#"
[project]
components_search_paths = ["./components"]
helm_chart_search_paths = ["./charts"]
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
