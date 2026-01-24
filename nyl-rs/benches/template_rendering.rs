use criterion::{black_box, criterion_group, criterion_main, Criterion};
use nyl::template::TemplateEngine;

fn bench_template_rendering(c: &mut Criterion) {
    c.bench_function("template_engine_new", |b| {
        b.iter(|| {
            let engine = TemplateEngine::new();
            black_box(engine);
        });
    });
}

criterion_group!(benches, bench_template_rendering);
criterion_main!(benches);
