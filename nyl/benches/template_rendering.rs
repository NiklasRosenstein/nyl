use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use nyl::template::TemplateEngine;
use serde::Deserialize;
use serde_json::json;

fn bench_template_creation(c: &mut Criterion) {
    c.bench_function("template_engine_new", |b| {
        b.iter(|| {
            let engine = TemplateEngine::new();
            black_box(engine);
        });
    });
}

fn bench_simple_template(c: &mut Criterion) {
    let engine = TemplateEngine::new();
    let context = json!({
        "name": "test",
        "namespace": "default",
    });

    c.bench_function("simple_template_render", |b| {
        b.iter(|| {
            let result = engine.render(
                black_box("name: {{ name }}, namespace: {{ namespace }}"),
                black_box(&context),
            );
            let _ = black_box(result);
        });
    });
}

fn bench_complex_template(c: &mut Criterion) {
    let engine = TemplateEngine::new();
    let context = json!({
        "name": "myapp",
        "namespace": "production",
        "replicas": 3,
        "labels": {
            "app": "myapp",
            "env": "production",
            "team": "platform",
        },
    });

    let template = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ name }}
  namespace: {{ namespace }}
  labels:
    {% for key, value in labels.items() %}
    {{ key }}: {{ value }}
    {% endfor %}
spec:
  replicas: {{ replicas }}
  selector:
    matchLabels:
      app: {{ name }}
  template:
    metadata:
      labels:
        app: {{ name }}
    spec:
      containers:
      - name: {{ name }}
        image: {{ name }}:latest
        ports:
        - containerPort: 8080
"#;

    c.bench_function("complex_template_render", |b| {
        b.iter(|| {
            let result = engine.render(black_box(template), black_box(&context));
            let _ = black_box(result);
        });
    });
}

fn bench_template_with_filters(c: &mut Criterion) {
    let engine = TemplateEngine::new();
    let context = json!({
        "data": "Hello, World!",
    });

    c.bench_function("template_with_b64encode", |b| {
        b.iter(|| {
            let result = engine.render(black_box("encoded: {{ data | b64encode }}"), black_box(&context));
            let _ = black_box(result);
        });
    });
}

fn bench_yaml_parsing(c: &mut Criterion) {
    let yaml_content = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
  namespace: default
data:
  key1: value1
  key2: value2
  key3: value3
"#;

    c.bench_function("yaml_parse_single_doc", |b| {
        b.iter(|| {
            let result: Result<serde_json::Value, _> = serde_norway::from_str(black_box(yaml_content));
            let _ = black_box(result);
        });
    });
}

fn bench_yaml_multi_doc(c: &mut Criterion) {
    let yaml_content = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: config1
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: config2
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: config3
"#;

    c.bench_function("yaml_parse_multi_doc", |b| {
        b.iter(|| {
            let docs: Vec<serde_json::Value> = serde_norway::Deserializer::from_str(black_box(yaml_content))
                .map(|de| serde_json::Value::deserialize(de).unwrap())
                .collect::<Vec<_>>();
            black_box(docs);
        });
    });
}

fn bench_json_serialization(c: &mut Criterion) {
    use serde_json::json;

    let data = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "test-config",
            "namespace": "default",
            "labels": {
                "app": "myapp",
                "env": "production",
            }
        },
        "data": {
            "key1": "value1",
            "key2": "value2",
        }
    });

    c.bench_function("json_to_yaml_serialization", |b| {
        b.iter(|| {
            let yaml = serde_norway::to_string(black_box(&data)).unwrap();
            black_box(yaml);
        });
    });
}

fn bench_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("template_scaling");

    for size in [10, 50, 100].iter() {
        let engine = TemplateEngine::new();

        // Create context with many keys
        let mut context_map = serde_json::Map::new();
        for i in 0..*size {
            context_map.insert(format!("key{i}"), json!(format!("value{i}")));
        }
        let context = json!(context_map);

        // Create template with many variables
        let mut template = String::new();
        for i in 0..*size {
            template.push_str(&format!("key{i}: {{{{ key{i} }}}}\n"));
        }

        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| {
                let result = engine.render(black_box(&template), black_box(&context));
                let _ = black_box(result);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_template_creation,
    bench_simple_template,
    bench_complex_template,
    bench_template_with_filters,
    bench_yaml_parsing,
    bench_yaml_multi_doc,
    bench_json_serialization,
    bench_scaling,
);
criterion_main!(benches);
