# Testing Strategy

## Overview

Comprehensive testing strategy for the Rust rewrite, ensuring correctness, reliability, and performance.

## Testing Pyramid

```
        /\
       /  \
      /E2E \        End-to-End Tests (10%)
     /______\       - Full CLI workflows
    /        \      - Real Helm charts
   /Integration\    Integration Tests (30%)
  /____________\    - Component interactions
 /              \   - File I/O, processes
/  Unit Tests   \   Unit Tests (60%)
/________________\  - Individual functions/modules
```

## Unit Testing

### Test Coverage Goals
- **Overall**: >80% code coverage
- **Core modules**: >90% coverage
  - Template engine
  - Configuration loading
  - Resource reconciliation
- **Generated code**: May exclude from coverage

### Unit Test Structure

```rust
// src/template/engine.rs

#[cfg(test)]
mod tests {
    use super::*;
    use minijinja::Value;

    #[test]
    fn test_render_simple_template() {
        let engine = TemplateEngine::new(Settings::default());
        let context = Value::from_serialize(serde_json::json!({
            "name": "test"
        })).unwrap();

        let result = engine.render("Hello {{ name }}!", &context).unwrap();
        assert_eq!(result, "Hello test!");
    }

    #[test]
    fn test_render_with_custom_function() {
        let engine = TemplateEngine::new(Settings::default());
        let context = Value::from_serialize(serde_json::json!({})).unwrap();

        let result = engine.render("{{ random_password(8) }}", &context).unwrap();
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_template_error_handling() {
        let engine = TemplateEngine::new(Settings::default());
        let context = Value::from_serialize(serde_json::json!({})).unwrap();

        let result = engine.render("{{ undefined_var }}", &context);
        assert!(result.is_err());
    }
}
```

### Key Areas for Unit Tests

1. **Template Functions**
   - Each custom function (random_password, randhex, b64encode, etc.)
   - Edge cases (empty input, invalid input)
   - Error conditions

2. **Configuration Parsing**
   - Valid configs (YAML, JSON)
   - Invalid configs (schema errors)
   - Missing optional fields
   - Default values

3. **Resource Handling**
   - Serialization/deserialization
   - Metadata handling
   - Field validation

4. **Hashing and Deduplication**
   - Stable hash generation
   - Hash consistency
   - Duplicate detection

5. **Utility Functions**
   - File system operations
   - String manipulation
   - Path handling

## Integration Testing

### Integration Test Structure

```rust
// tests/template_integration.rs

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use std::fs;

#[tokio::test]
async fn test_template_basic_manifest() {
    // Setup
    let temp = TempDir::new().unwrap();
    let manifest = temp.path().join("manifest.yaml");

    fs::write(&manifest, r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: ${{ name }}
data:
  key: value
"#).unwrap();

    let project_config = temp.path().join("nyl-project.yaml");
    fs::write(&project_config, r#"
settings:
  componentsPath: components
"#).unwrap();

    // Execute
    let mut cmd = Command::cargo_bin("nyl-rs").unwrap();
    cmd.arg("template")
        .arg(&manifest)
        .arg("-C")
        .arg(temp.path())
        .arg("--set")
        .arg("name=test-config");

    // Assert
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("name: test-config"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}
```

### Integration Test Scenarios

1. **CLI Commands**
   - `nyl new` creates project correctly
   - `nyl template` renders manifests
   - `nyl validate` catches errors
   - Error messages are helpful

2. **File Operations**
   - Config discovery (upward search)
   - Multi-file manifest loading
   - Output to file vs stdout

3. **Helm Integration**
   - Real Helm chart rendering
   - Chart caching
   - Multiple charts in parallel
   - Chart from different sources (HTTP, OCI, local)

4. **Multi-Document YAML**
   - Load and process multiple documents
   - Preserve document order
   - Handle separator lines

5. **Complex Workflows**
   - Templates → HelmCharts → Resources
   - Multiple levels of nesting
   - Deduplication across levels

## End-to-End Testing

### E2E Test Structure

```rust
// tests/e2e/full_workflow.rs

#[tokio::test]
async fn test_complete_application_workflow() {
    // 1. Create new project
    let temp = TempDir::new().unwrap();

    Command::cargo_bin("nyl-rs").unwrap()
        .arg("new")
        .arg("test-project")
        .arg("--path")
        .arg(temp.path())
        .assert()
        .success();

    // 2. Add Helm chart manifest
    let manifest_path = temp.path().join("manifests").join("postgres.yaml");
    fs::write(&manifest_path, POSTGRES_HELMCHART_YAML).unwrap();

    // 3. Render manifests
    let output = Command::cargo_bin("nyl-rs").unwrap()
        .arg("template")
        .arg(&manifest_path)
        .arg("-C")
        .arg(temp.path())
        .output()
        .unwrap();

    // 4. Verify output contains expected resources
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("kind: StatefulSet"));
    assert!(stdout.contains("kind: Service"));
    assert!(stdout.contains("app.kubernetes.io/name: postgresql"));

    // 5. Validate output is valid YAML
    let resources: Vec<serde_yaml::Value> =
        serde_yaml::Deserializer::from_str(&stdout)
            .map(|doc| serde_yaml::Value::deserialize(doc).unwrap())
            .collect();

    assert!(resources.len() > 0);
}
```

### E2E Test Scenarios

1. **Complete Project Lifecycle**
   - Create project
   - Add manifests
   - Render templates
   - Validate output

2. **Real-World Examples**
   - Deploy PostgreSQL with Helm
   - Deploy complex application stack
   - Multi-environment configuration

3. **Performance Scenarios**
   - Large number of resources (1000+)
   - Many Helm charts in parallel (10+)
   - Deep nesting levels

## Performance Testing

### Benchmark Structure

```rust
// benches/rendering.rs

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use nyl::template::TemplateEngine;

fn bench_template_rendering(c: &mut Criterion) {
    let engine = TemplateEngine::new(Settings::default());
    let context = create_test_context();

    c.bench_function("render simple template", |b| {
        b.iter(|| {
            engine.render(
                black_box("Hello {{ name }}!"),
                black_box(&context)
            )
        })
    });
}

fn bench_yaml_rendering(c: &mut Criterion) {
    let engine = TemplateEngine::new(Settings::default());
    let yaml = include_str!("../fixtures/complex-manifest.yaml");
    let context = create_test_context();

    c.bench_function("render complex YAML", |b| {
        b.iter(|| {
            engine.render_yaml(
                black_box(yaml),
                black_box(&context)
            )
        })
    });
}

fn bench_helm_chart(c: &mut Criterion) {
    let generator = HelmChartGenerator::new(cache_dir());
    let chart = create_test_helmchart();
    let context = create_test_context();

    c.bench_function("render Helm chart", |b| {
        b.to_async(Runtime::new().unwrap()).iter(|| async {
            generator.render_chart(
                black_box(&chart),
                black_box(&context)
            ).await
        })
    });
}

criterion_group!(
    benches,
    bench_template_rendering,
    bench_yaml_rendering,
    bench_helm_chart
);
criterion_main!(benches);
```

### Performance Benchmarks

1. **Template Rendering**
   - Simple templates
   - Complex templates with functions
   - Large templates (>10KB)

2. **YAML Processing**
   - Small manifests (<1KB)
   - Medium manifests (1-10KB)
   - Large manifests (>10KB)
   - Multi-document YAML

3. **Helm Chart Rendering**
   - Single chart
   - Multiple charts (parallel)
   - Chart download and caching

4. **Resource Reconciliation**
   - Few resources (10)
   - Many resources (100)
   - Very many resources (1000+)

5. **Memory Usage**
   - Peak memory consumption
   - Memory over time
   - Leak detection

### Performance Targets

| Benchmark | Target | Baseline (Python) |
|-----------|--------|-------------------|
| Simple template | <10μs | ~100μs |
| Complex YAML | <1ms | ~10ms |
| Helm chart | <500ms | ~2s |
| 100 resources | <100ms | ~1s |
| 1000 resources | <1s | ~10s |
| Memory (100 charts) | <50MB | ~200MB |

## Test Fixtures

### Fixture Organization

```
tests/
└── fixtures/
    ├── configs/
    │   ├── valid-minimal.yaml
    │   ├── valid-complete.yaml
    │   ├── invalid-schema.yaml
    │   └── invalid-yaml.yaml
    ├── manifests/
    │   ├── simple-configmap.yaml
    │   ├── deployment-with-template.yaml
    │   └── multi-document.yaml
    ├── helmcharts/
    │   ├── postgres.yaml
    │   ├── redis.yaml
    │   └── custom-chart/
    └── expected/
        ├── simple-configmap.yaml
        ├── rendered-deployment.yaml
        └── postgres-manifests.yaml
```

### Fixture Management

```rust
// tests/common/fixtures.rs

use std::path::{Path, PathBuf};

pub struct Fixtures;

impl Fixtures {
    pub fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    pub fn config(name: &str) -> PathBuf {
        Self::root().join("configs").join(name)
    }

    pub fn manifest(name: &str) -> PathBuf {
        Self::root().join("manifests").join(name)
    }

    pub fn helmchart(name: &str) -> PathBuf {
        Self::root().join("helmcharts").join(name)
    }

    pub fn load_expected(name: &str) -> String {
        std::fs::read_to_string(
            Self::root().join("expected").join(name)
        ).unwrap()
    }
}
```

## Property-Based Testing

Use `proptest` for property-based testing:

```toml
[dev-dependencies]
proptest = "1.6"
```

```rust
// src/util/hash.rs

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn test_stable_hash_deterministic(resource in any_resource()) {
            // Same resource should always produce same hash
            let hash1 = stable_hash(&resource).unwrap();
            let hash2 = stable_hash(&resource).unwrap();
            assert_eq!(hash1, hash2);
        }

        #[test]
        fn test_stable_hash_differs_for_different_resources(
            r1 in any_resource(),
            r2 in any_resource()
        ) {
            // Different resources (likely) produce different hashes
            prop_assume!(r1 != r2);
            let hash1 = stable_hash(&r1).unwrap();
            let hash2 = stable_hash(&r2).unwrap();
            assert_ne!(hash1, hash2);
        }
    }

    fn any_resource() -> impl Strategy<Value = Resource> {
        // Generate arbitrary valid resources
        (any::<String>(), any::<String>(), any::<String>())
            .prop_map(|(api_version, kind, name)| {
                Resource {
                    api_version,
                    kind,
                    metadata: Metadata {
                        name,
                        ..Default::default()
                    },
                    data: Default::default(),
                }
            })
    }
}
```

## Continuous Integration

### CI Pipeline

```yaml
# .github/workflows/test.yml

name: Test

on: [push, pull_request]

jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
        rust: [stable, beta]

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@master
        with:
          toolchain: ${{ matrix.rust }}
          components: clippy, rustfmt

      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}

      - name: Install Helm (for integration tests)
        run: |
          curl https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash

      - name: Run tests
        run: cargo test --all-features

      - name: Run clippy
        run: cargo clippy --all-features -- -D warnings

      - name: Check formatting
        run: cargo fmt -- --check

  coverage:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install tarpaulin
        run: cargo install cargo-tarpaulin

      - name: Generate coverage
        run: cargo tarpaulin --out Xml --all-features

      - name: Upload coverage
        uses: codecov/codecov-action@v4
        with:
          files: ./cobertura.xml

  benchmark:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Run benchmarks
        run: cargo bench --all-features

      - name: Store benchmark results
        uses: benchmark-action/github-action-benchmark@v1
        with:
          tool: 'cargo'
          output-file-path: target/criterion/output.txt
```

## Test Data Generation

### Snapshot Testing

Use `insta` for snapshot testing:

```toml
[dev-dependencies]
insta = "1.41"
```

```rust
// tests/snapshots.rs

use insta::assert_yaml_snapshot;

#[test]
fn test_render_postgres_chart() {
    let output = render_postgres_chart();
    assert_yaml_snapshot!(output);
}
```

## Error Case Testing

### Comprehensive Error Testing

```rust
#[test]
fn test_invalid_yaml() {
    let engine = TemplateEngine::new(Settings::default());
    let result = engine.render_yaml("invalid: yaml: structure:", &Value::UNDEFINED);
    assert!(result.is_err());
}

#[test]
fn test_missing_helm_binary() {
    let generator = HelmChartGenerator::new_with_binary(
        cache_dir(),
        PathBuf::from("/nonexistent/helm")
    );

    let result = generator.render_chart(&test_chart(), &test_context()).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("helm binary not found"));
}
```

## Test Quality Metrics

### Metrics to Track

1. **Coverage**: >80% line coverage
2. **Test Count**: >500 tests total
3. **Test Speed**: <30s for full test suite
4. **Flakiness**: <1% flaky tests
5. **Maintainability**: Tests updated with code changes

## Summary

The testing strategy ensures:
- ✅ High code coverage (>80%)
- ✅ Fast feedback (<30s test suite)
- ✅ Cross-platform validation
- ✅ Performance regression detection
- ✅ Real-world scenario coverage
- ✅ Comprehensive error handling
