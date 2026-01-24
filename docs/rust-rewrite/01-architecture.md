# Architecture Design

## Overview

The Rust rewrite adopts a layered architecture with clear separation of concerns, optimized for performance, maintainability, and extensibility.

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     CLI Layer (Clap)                        │
│  Commands: render, diff, apply, new, validate              │
└────────────────┬────────────────────────────────────────────┘
                 │
┌────────────────▼────────────────────────────────────────────┐
│              Configuration Layer                            │
│  - Project config loading (YAML/JSON/TOML)                 │
│  - Validation and schema enforcement                        │
│  - Environment variable expansion                           │
└────────────────┬────────────────────────────────────────────┘
                 │
┌────────────────▼────────────────────────────────────────────┐
│              Template Engine Layer                          │
│  - Jinja2-compatible template parser (MiniJinja)           │
│  - Custom functions registry                                │
│  - Context management                                       │
└────────────────┬────────────────────────────────────────────┘
                 │
┌────────────────▼────────────────────────────────────────────┐
│              Generator Layer                                │
│  - Resource dispatching                                     │
│  - HelmChart generator (subprocess to helm)                │
│  - Component discovery and generation                       │
│  - Resource reconciliation and deduplication               │
└────────────────┬────────────────────────────────────────────┘
                 │
┌────────────────▼────────────────────────────────────────────┐
│              Kubernetes Layer                               │
│  - Resource type definitions (kube-rs)                     │
│  - YAML/JSON serialization                                 │
│  - API version handling                                     │
└────────────────┬────────────────────────────────────────────┘
                 │
┌────────────────▼────────────────────────────────────────────┐
│              Output Layer                                   │
│  - YAML formatting and emission                            │
│  - Namespace defaulting                                     │
│  - Multi-document output                                    │
└─────────────────────────────────────────────────────────────┘
```

## Core Design Principles

### 1. Trait-Based Extensibility
Use Rust traits to define interfaces for generators, providers, and plugins:

```rust
/// Core trait for resource generators
pub trait Generator: Send + Sync {
    /// The input resource type this generator handles
    type Input: DeserializeOwned;

    /// The output resource types this generator produces
    type Output: Serialize;

    /// Check if this generator can handle the given resource
    fn can_handle(&self, resource: &Resource) -> bool;

    /// Generate resources from the input
    async fn generate(&self, input: Self::Input, ctx: &Context) -> Result<Vec<Self::Output>>;
}
```

### 2. Async-First Design
Leverage Tokio for concurrent operations:
- Parallel Helm chart rendering
- Concurrent file I/O
- Async HTTP requests for chart repositories
- Parallel template evaluation (where possible)

### 3. Zero-Copy Where Possible
Minimize allocations and copies:
- Use `Cow<str>` for string data that may be borrowed or owned
- Reference-based API design
- Efficient YAML parsing with streaming where applicable

### 4. Error Handling
Strong, typed error handling using `thiserror`:

```rust
#[derive(Error, Debug)]
pub enum NylError {
    #[error("Template error: {0}")]
    Template(#[from] minijinja::Error),

    #[error("Helm chart error: {0}")]
    HelmChart(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_norway::Error),
}

pub type Result<T> = std::result::Result<T, NylError>;
```

### 5. Configuration as Code
Use serde for configuration with strong typing:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    #[serde(default)]
    pub settings: Settings,

    #[serde(default)]
    pub components_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_true")]
    pub generate_applysets: bool,

    #[serde(default)]
    pub on_lookup_failure: LookupFailureMode,
}
```

## Module Structure

```
nyl/
├── src/
│   ├── main.rs                 # Entry point
│   ├── lib.rs                  # Library exports
│   │
│   ├── cli/                    # CLI layer
│   │   ├── mod.rs
│   │   ├── commands/
│   │   │   ├── render.rs       # Render command (to stdout)
│   │   │   ├── diff.rs         # Diff command (kubectl diff)
│   │   │   ├── apply.rs        # Apply command (kubectl apply)
│   │   │   ├── new.rs          # New project scaffolding
│   │   │   └── validate.rs     # Config validation
│   │   └── output.rs           # Output formatting
│   │
│   ├── config/                 # Configuration layer
│   │   ├── mod.rs
│   │   ├── project.rs          # Project configuration
│   │   ├── loader.rs           # Config file loading
│   │   └── schema.rs           # Configuration schema
│   │
│   ├── template/               # Template engine layer
│   │   ├── mod.rs
│   │   ├── engine.rs           # MiniJinja wrapper
│   │   ├── functions.rs        # Custom template functions
│   │   └── filters.rs          # Custom filters
│   │
│   ├── generator/              # Generator layer
│   │   ├── mod.rs
│   │   ├── dispatch.rs         # Generator dispatcher
│   │   ├── helm.rs             # Helm chart generator
│   │   ├── component.rs        # Component generator
│   │   ├── reconcile.rs        # Resource reconciliation
│   │   └── cache.rs            # Chart and repo caching
│   │
│   ├── kubernetes/             # Kubernetes layer
│   │   ├── mod.rs
│   │   ├── resource.rs         # Generic resource type
│   │   ├── types.rs            # Common K8s types
│   │   └── yaml.rs             # YAML serialization
│   │
│   ├── resources/              # Custom Nyl resources
│   │   ├── mod.rs
│   │   ├── helmchart.rs        # HelmChart resource
│   │   └── component.rs        # Component resource
│   │
│   └── util/                   # Utilities
│       ├── mod.rs
│       ├── hash.rs             # Stable hashing
│       ├── fs.rs               # File system utilities
│       └── process.rs          # External process handling
│
├── tests/                      # Integration tests
│   ├── template_test.rs
│   ├── helm_test.rs
│   └── fixtures/
│
├── benches/                    # Benchmarks
│   └── rendering.rs
│
├── Cargo.toml
└── README.md
```

## Key Components

### 1. Template Engine
**Responsibility**: Evaluate Jinja2-compatible templates

**Design**:
- Wrapper around MiniJinja for template evaluation
- Custom function registry using MiniJinja's extensibility
- Context management for variables and functions
- Support for both string and structured (YAML) templates
- Uses serde-norway for YAML parsing (YAML 1.2 compliant)

**Interface**:
```rust
pub struct TemplateEngine {
    env: minijinja::Environment<'static>,
}

impl TemplateEngine {
    pub fn new() -> Self;
    pub fn register_function<F>(&mut self, name: &str, func: F);
    pub fn render(&self, template: &str, context: &Context) -> Result<String>;
    pub fn render_yaml(&self, yaml: &str, context: &Context) -> Result<Vec<Resource>>;
}
```

### 2. Generator Dispatcher
**Responsibility**: Route resources to appropriate generators

**Design**:
- Trait-based generator registry
- Resource kind/apiVersion matching
- Topological sorting for dependency resolution
- Deduplication using stable hashing

**Interface**:
```rust
pub struct GeneratorDispatcher {
    generators: Vec<Box<dyn Generator>>,
}

impl GeneratorDispatcher {
    pub fn new() -> Self;
    pub fn register<G: Generator + 'static>(&mut self, generator: G);
    pub async fn generate(&self, resources: Vec<Resource>, ctx: &Context) -> Result<Vec<Resource>>;
}
```

### 3. Helm Chart Generator
**Responsibility**: Render Helm charts via helm CLI

**Design**:
- Subprocess invocation to `helm template`
- Chart repository management (HTTP, OCI, Git)
- Local caching of downloaded charts
- Parallel chart rendering using async/await

**Interface**:
```rust
pub struct HelmChartGenerator {
    cache_dir: PathBuf,
    helm_bin: PathBuf,
}

impl Generator for HelmChartGenerator {
    type Input = HelmChart;
    type Output = Resource;

    async fn generate(&self, chart: Self::Input, ctx: &Context) -> Result<Vec<Self::Output>> {
        // 1. Resolve chart (download/cache if needed)
        // 2. Construct helm template command
        // 3. Execute helm and capture output
        // 4. Parse YAML resources
        // 5. Return resources
    }
}
```

### 4. Resource Reconciliation
**Responsibility**: Deduplicate and merge resources

**Design**:
- Recursive generator invocation
- Stable hashing for deduplication (same as Python)
- Reference resolution
- Cycle detection

**Algorithm**:
```rust
pub async fn reconcile(
    initial_resources: Vec<Resource>,
    dispatcher: &GeneratorDispatcher,
    ctx: &Context,
) -> Result<Vec<Resource>> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from(initial_resources);
    let mut output = Vec::new();

    while let Some(resource) = queue.pop_front() {
        let hash = stable_hash(&resource);
        if seen.contains(&hash) {
            continue; // Skip duplicates
        }
        seen.insert(hash);

        if dispatcher.can_generate(&resource) {
            let generated = dispatcher.generate(resource, ctx).await?;
            queue.extend(generated);
        } else {
            output.push(resource);
        }
    }

    Ok(output)
}
```

## Performance Optimizations

### 1. Parallel Processing
- Use `tokio::spawn` for parallel Helm chart rendering
- Use `rayon` for CPU-bound parallel template evaluation
- Async I/O for file operations and HTTP requests

### 2. Caching Strategy
- **Chart Cache**: Downloaded Helm charts cached by (repo, name, version)
- **Git Cache**: Git repositories cached with shallow clones
- **Template Cache**: Compiled templates cached in memory
- **Hash Cache**: Stable hashes memoized during reconciliation

### 3. Memory Management
- Stream large YAML files instead of loading entirely in memory
- Use `Box<str>` instead of `String` for immutable strings
- Implement `Drop` for cache cleanup
- Use `Arc` for shared immutable data across async tasks

### 4. Binary Size Optimization
- Use feature flags to conditionally compile features
- Strip debug symbols in release builds
- Use `lto = "thin"` for link-time optimization
- Consider UPX compression for final binary

## Comparison with Python Architecture

### Improvements
1. **Type Safety**: Compile-time guarantees vs runtime checks
2. **Performance**: Native code vs interpreted Python
3. **Concurrency**: True parallelism vs GIL-limited threading
4. **Dependencies**: Single binary vs pip packages
5. **Memory**: Explicit ownership vs reference counting + GC

### Simplifications
1. **No Daemon**: Remove daemon functionality (not needed with fast startup)
2. **Simpler DI**: Use constructor injection instead of complex DI framework
3. **Fewer Abstractions**: Direct implementations where Python used metaclasses
4. **Unified Config**: Single configuration format instead of YAML/TOML/JSON splits

### Deferred Features
1. **Secrets Providers**: Start with environment variables only
2. **Profile Management**: Defer SSH tunnels and complex kubeconfig handling
3. **ApplySet Generation**: Implement in later phase
4. **Post-Processing**: Defer Kyverno integration
5. **kubectl Integration**: Focus on YAML output first
