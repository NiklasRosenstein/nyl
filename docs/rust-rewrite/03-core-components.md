# Core Components - Detailed Design

## Overview

This document provides detailed implementation specifications for each core component of the Rust rewrite.

## 1. CLI Layer

### 1.1 Command Structure

```rust
// src/cli/mod.rs

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nyl",
    version,
    about = "Fast Kubernetes manifest generation and templating",
    long_about = "Nyl is a high-performance tool for generating Kubernetes manifests \
                  from templates, with built-in support for Helm charts and composition."
)]
pub struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Logging level (error, warn, info, debug, trace)
    #[arg(long, global = true, default_value = "info")]
    pub log_level: String,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Render templates and generate Kubernetes manifests
    Template(TemplateArgs),

    /// Create a new Nyl project
    New(NewArgs),

    /// Validate configuration files
    Validate(ValidateArgs),
}
```

### 1.2 Render Command

```rust
// src/cli/commands/render.rs

use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct RenderArgs {
    /// Input files or directories to process
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Project directory (defaults to current directory)
    #[arg(short = 'C', long)]
    pub project_dir: Option<PathBuf>,

    /// Values to set (can be used multiple times)
    #[arg(short, long = "set")]
    pub values: Vec<String>,

    /// Values files to load (YAML)
    #[arg(short = 'f', long = "values")]
    pub values_files: Vec<PathBuf>,

    /// Output file (defaults to stdout)
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Pretty print output (multi-line YAML)
    #[arg(long)]
    pub pretty: bool,
}

pub async fn execute(args: RenderArgs) -> anyhow::Result<()> {
    // 1. Load project configuration
    let project = load_project_config(&args.project_dir)?;

    // 2. Load values from files and CLI args
    let values = build_values_context(&args)?;

    // 3. Discover and load manifest files
    let manifests = discover_manifests(&args.files)?;

    // 4. Initialize template engine
    let engine = TemplateEngine::new(project.settings.clone());

    // 5. Render manifests
    let resources = engine.render_manifests(manifests, &values).await?;

    // 6. Initialize generator dispatcher
    let dispatcher = GeneratorDispatcher::new();

    // 7. Reconcile resources (expand HelmCharts, etc.)
    let final_resources = reconcile(resources, &dispatcher, &values).await?;

    // 8. Format and output
    output_resources(&final_resources, &args)?;

    Ok(())
}
```

### 1.3 Diff Command

```rust
// src/cli/commands/diff.rs

#[derive(Args)]
pub struct DiffArgs {
    /// Input files or directories to process
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Project directory (defaults to current directory)
    #[arg(short = 'C', long)]
    pub project_dir: Option<PathBuf>,

    /// Values to set (can be used multiple times)
    #[arg(short, long = "set")]
    pub values: Vec<String>,

    /// Values files to load (YAML)
    #[arg(short = 'f', long = "values")]
    pub values_files: Vec<PathBuf>,

    /// Context lines to show around changes
    #[arg(short = 'c', long, default_value = "3")]
    pub context: usize,
}

pub async fn execute(args: DiffArgs) -> anyhow::Result<()> {
    // 1. Render manifests (same as render command)
    let resources = render_manifests(&args).await?;

    // 2. Call kubectl diff with rendered YAML
    let kubectl_diff = Command::new("kubectl")
        .arg("diff")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    // 3. Pipe rendered YAML to kubectl
    let mut stdin = kubectl_diff.stdin.as_ref().unwrap();
    write_resources(&mut stdin, &resources)?;

    // 4. Display diff output
    let output = kubectl_diff.wait_with_output()?;
    print!("{}", String::from_utf8_lossy(&output.stdout));

    Ok(())
}
```

### 1.4 Apply Command

```rust
// src/cli/commands/apply.rs

#[derive(Args)]
pub struct ApplyArgs {
    /// Input files or directories to process
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Project directory (defaults to current directory)
    #[arg(short = 'C', long)]
    pub project_dir: Option<PathBuf>,

    /// Values to set (can be used multiple times)
    #[arg(short, long = "set")]
    pub values: Vec<String>,

    /// Values files to load (YAML)
    #[arg(short = 'f', long = "values")]
    pub values_files: Vec<PathBuf>,

    /// Dry run (client-side only)
    #[arg(long)]
    pub dry_run: bool,

    /// Wait for resources to be ready
    #[arg(long)]
    pub wait: bool,

    /// Timeout for wait (in seconds)
    #[arg(long, default_value = "300")]
    pub timeout: u64,
}

pub async fn execute(args: ApplyArgs) -> anyhow::Result<()> {
    // 1. Render manifests
    let resources = render_manifests(&args).await?;

    // 2. Build kubectl apply command
    let mut cmd = Command::new("kubectl");
    cmd.arg("apply").arg("-f").arg("-");

    if args.dry_run {
        cmd.arg("--dry-run=client");
    }

    if args.wait {
        cmd.arg("--wait")
            .arg("--timeout")
            .arg(format!("{}s", args.timeout));
    }

    // 3. Pipe rendered YAML to kubectl
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    write_resources(&mut stdin, &resources)?;
    drop(stdin);

    // 4. Wait for completion and show output
    let output = child.wait_with_output()?;

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        anyhow::bail!("kubectl apply failed");
    }

    Ok(())
}
```

### 1.5 New Command

```rust
// src/cli/commands/new.rs

#[derive(Args)]
pub struct NewArgs {
    /// Project name
    pub name: String,

    /// Project directory (defaults to ./{name})
    #[arg(short, long)]
    pub path: Option<PathBuf>,

    /// Initialize with example manifests
    #[arg(long)]
    pub with_examples: bool,
}

pub async fn execute(args: NewArgs) -> anyhow::Result<()> {
    // 1. Create project directory
    let project_dir = args.path.unwrap_or_else(|| PathBuf::from(&args.name));
    tokio::fs::create_dir_all(&project_dir).await?;

    // 2. Generate nyl-project.yaml
    let config = ProjectConfig::default();
    let config_yaml = serde_yaml::to_string(&config)?;
    tokio::fs::write(
        project_dir.join("nyl-project.yaml"),
        config_yaml
    ).await?;

    // 3. Create components directory
    tokio::fs::create_dir(project_dir.join("components")).await?;

    // 4. Create manifests directory
    tokio::fs::create_dir(project_dir.join("manifests")).await?;

    // 5. Optionally create examples
    if args.with_examples {
        create_example_manifests(&project_dir).await?;
    }

    println!("Created new Nyl project: {}", args.name);
    Ok(())
}
```

## 2. Configuration Layer

### 2.1 Project Configuration

```rust
// src/config/project.rs

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    #[serde(default)]
    pub settings: Settings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Automatically generate ApplySets for resources (future feature)
    #[serde(default = "default_false")]
    pub generate_applysets: bool,

    /// Directory for Nyl components
    #[serde(default = "default_components_path")]
    pub components_path: PathBuf,

    /// Search paths for resource files
    #[serde(default = "default_search_path")]
    pub search_path: Vec<PathBuf>,
}

fn default_false() -> bool { false }
fn default_components_path() -> PathBuf { PathBuf::from("components") }
fn default_search_path() -> Vec<PathBuf> { vec![PathBuf::from(".")] }

impl Default for Settings {
    fn default() -> Self {
        Self {
            generate_applysets: false,
            components_path: default_components_path(),
            search_path: default_search_path(),
        }
    }
}
```

### 2.2 Configuration Loader

```rust
// src/config/loader.rs

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};

pub struct ConfigLoader;

impl ConfigLoader {
    /// Find and load project configuration
    /// Searches upward from current directory for nyl-project.{yaml,yml,json,toml}
    pub async fn load(start_dir: Option<&Path>) -> Result<ProjectConfig> {
        let start = start_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().expect("current dir"));

        let config_path = Self::find_config_file(&start)
            .ok_or_else(|| anyhow::anyhow!("No nyl-project config file found"))?;

        Self::load_from_file(&config_path).await
    }

    /// Find config file by searching upward
    fn find_config_file(start: &Path) -> Option<PathBuf> {
        let mut current = start;

        loop {
            for name in &["nyl-project.yaml", "nyl-project.yml", "nyl-project.json"] {
                let path = current.join(name);
                if path.exists() {
                    return Some(path);
                }
            }

            // Move up one directory
            current = current.parent()?;
        }
    }

    /// Load and parse config file
    async fn load_from_file(path: &Path) -> Result<ProjectConfig> {
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read config: {}", path.display()))?;

        match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => {
                serde_yaml::from_str(&content)
                    .with_context(|| format!("Failed to parse YAML config: {}", path.display()))
            }
            Some("json") => {
                serde_json::from_str(&content)
                    .with_context(|| format!("Failed to parse JSON config: {}", path.display()))
            }
            _ => Err(anyhow::anyhow!("Unsupported config format: {}", path.display())),
        }
    }
}
```

## 3. Template Engine Layer

### 3.1 Template Engine

```rust
// src/template/engine.rs

use minijinja::{Environment, Value};
use std::sync::Arc;
use anyhow::Result;

pub struct TemplateEngine {
    env: Environment<'static>,
    settings: Settings,
}

impl TemplateEngine {
    pub fn new(settings: Settings) -> Self {
        let mut env = Environment::new();

        // Register custom functions
        functions::register_all(&mut env);

        // Register custom filters
        filters::register_all(&mut env);

        Self { env, settings }
    }

    /// Render a template string with given context
    pub fn render(&self, template: &str, context: &Value) -> Result<String> {
        let tmpl = self.env.template_from_str(template)?;
        Ok(tmpl.render(context)?)
    }

    /// Render YAML with embedded templates
    pub fn render_yaml(&self, yaml: &str, context: &Value) -> Result<Vec<Resource>> {
        // 1. Parse YAML into Value
        let mut docs: Vec<serde_norway::Value> = serde_norway::Deserializer::from_str(yaml)
            .map(|doc| serde_norway::Value::deserialize(doc))
            .collect::<Result<_, _>>()?;

        // 2. Recursively evaluate templates in YAML structure
        for doc in &mut docs {
            self.eval_yaml_recursive(doc, context)?;
        }

        // 3. Convert to Resource structs
        let resources = docs
            .into_iter()
            .map(|doc| serde_norway::from_value(doc))
            .collect::<Result<Vec<Resource>, _>>()?;

        Ok(resources)
    }

    /// Recursively evaluate template expressions in YAML
    fn eval_yaml_recursive(&self, value: &mut serde_norway::Value, context: &Value) -> Result<()> {
        match value {
            serde_norway::Value::String(s) => {
                // Check if string contains template syntax: ${{ ... }}
                if s.contains("${{") {
                    *s = self.render(s, context)?;
                }
            }
            serde_norway::Value::Mapping(map) => {
                for (_, v) in map.iter_mut() {
                    self.eval_yaml_recursive(v, context)?;
                }
            }
            serde_norway::Value::Sequence(seq) => {
                for v in seq.iter_mut() {
                    self.eval_yaml_recursive(v, context)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Render multiple manifest files
    pub async fn render_manifests(
        &self,
        manifests: Vec<PathBuf>,
        context: &Value,
    ) -> Result<Vec<Resource>> {
        let mut all_resources = Vec::new();

        for manifest_path in manifests {
            let content = tokio::fs::read_to_string(&manifest_path).await?;
            let resources = self.render_yaml(&content, context)?;
            all_resources.extend(resources);
        }

        Ok(all_resources)
    }
}
```

### 3.2 Custom Functions

```rust
// src/template/functions.rs

use minijinja::{Environment, Value};
use rand::Rng;
use base64::{Engine as _, engine::general_purpose};

pub fn register_all(env: &mut Environment) {
    env.add_function("random_password", random_password);
    env.add_function("randhex", randhex);
    env.add_function("b64encode", b64encode);
    env.add_function("b64decode", b64decode);
    env.add_function("ref", reference);
}

/// Generate random password
fn random_password(length: Option<usize>) -> String {
    let len = length.unwrap_or(32);
    let mut rng = rand::thread_rng();

    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                         abcdefghijklmnopqrstuvwxyz\
                         0123456789";

/// Generate random hex string
fn randhex(length: Option<usize>) -> String {
    let len = length.unwrap_or(32);
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| format!("{:x}", rng.gen_range(0..16)))
        .collect()
}

/// Base64 encode
fn b64encode(input: String) -> String {
    general_purpose::STANDARD.encode(input.as_bytes())
}

/// Base64 decode
fn b64decode(input: String) -> Result<String, minijinja::Error> {
    let bytes = general_purpose::STANDARD
        .decode(input.as_bytes())
        .map_err(|e| minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("Base64 decode error: {}", e)
        ))?;

    String::from_utf8(bytes)
        .map_err(|e| minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("UTF-8 decode error: {}", e)
        ))
}

/// Reference another resource field (placeholder for now)
fn reference(kind: String, name: String, field: String) -> Value {
    // TODO: Implement actual lookup in future phase
    Value::from(format!("${{ref:{}:{}:{}}}", kind, name, field))
}
```

## 4. Generator Layer

### 4.1 Generator Trait

```rust
// src/generator/mod.rs

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use crate::kubernetes::Resource;

#[async_trait]
pub trait Generator: Send + Sync {
    /// Check if this generator can handle the resource
    fn can_handle(&self, resource: &Resource) -> bool;

    /// Generate resources from input
    async fn generate(
        &self,
        resource: Resource,
        context: &Context,
    ) -> Result<Vec<Resource>>;
}

pub struct Context {
    pub values: Value,
    pub project_dir: PathBuf,
    pub cache_dir: PathBuf,
}
```

### 4.2 HelmChart Generator

```rust
// src/generator/helm.rs

use async_trait::async_trait;
use crate::resources::HelmChart;
use crate::generator::{Generator, Context};
use tokio::process::Command;

pub struct HelmChartGenerator {
    cache_dir: PathBuf,
    helm_binary: PathBuf,
}

impl HelmChartGenerator {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            helm_binary: PathBuf::from("helm"),
        }
    }

    async fn render_chart(
        &self,
        chart: &HelmChart,
        context: &Context,
    ) -> Result<Vec<Resource>> {
        // 1. Resolve chart location (download if needed)
        let chart_path = self.resolve_chart(chart).await?;

        // 2. Prepare values
        let values_yaml = serde_norway::to_string(&chart.spec.values)?;
        let values_file = tempfile::NamedTempFile::new()?;
        tokio::fs::write(values_file.path(), values_yaml).await?;

        // 3. Build helm template command
        let release_name = chart.spec.release.as_ref()
            .and_then(|r| r.name.as_deref())
            .unwrap_or(&chart.metadata.name);

        let mut cmd = Command::new(&self.helm_binary);
        cmd.arg("template")
            .arg(release_name)
            .arg(&chart_path)
            .arg("--values")
            .arg(values_file.path());

        // Add namespace if specified
        if let Some(namespace) = chart.spec.release.as_ref()
            .and_then(|r| r.namespace.as_deref()) {
            cmd.arg("--namespace").arg(namespace);
        }

        // 4. Execute helm
        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Helm template failed: {}", stderr);
        }

        // 5. Parse output YAML
        let stdout = String::from_utf8(output.stdout)?;
        let resources: Vec<Resource> = serde_norway::Deserializer::from_str(&stdout)
            .map(|doc| serde_norway::from_value(serde_norway::Value::deserialize(doc)?))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(resources)
    }

    async fn resolve_chart(&self, chart: &HelmChart) -> Result<PathBuf> {
        match &chart.spec.chart {
            ChartRef::Repository { repository, name, version } => {
                // Download from repository (OCI or HTTP)
                self.download_from_repo(repository, name, version).await
            }
            ChartRef::Path(path) => {
                // Use local path
                Ok(path.clone())
            }
        }
    }

    async fn download_from_repo(
        &self,
        repo: &str,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        // Cache key: sha256(repo + name + version)
        let cache_key = format!("{}-{}-{}", repo, name, version);
        let cache_path = self.cache_dir.join(&cache_key);

        // Check cache
        if cache_path.exists() {
            return Ok(cache_path);
        }

        // Download using helm pull
        let mut cmd = Command::new(&self.helm_binary);
        cmd.arg("pull")
            .arg(format!("{}/{}", repo, name))
            .arg("--version")
            .arg(version)
            .arg("--untar")
            .arg("--untardir")
            .arg(&cache_path);

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Helm pull failed: {}", stderr);
        }

        Ok(cache_path.join(name))
    }
}

#[async_trait]
impl Generator for HelmChartGenerator {
    fn can_handle(&self, resource: &Resource) -> bool {
        resource.api_version == "nyl.io/v1" && resource.kind == "HelmChart"
    }

    async fn generate(&self, resource: Resource, context: &Context) -> Result<Vec<Resource>> {
        let chart: HelmChart = serde_norway::from_value(serde_norway::to_value(&resource)?)?;
        self.render_chart(&chart, context).await
    }
}
```

### 4.3 Resource Reconciliation

```rust
// src/generator/reconcile.rs

use std::collections::{HashSet, VecDeque};
use crate::generator::{GeneratorDispatcher, Context};
use crate::kubernetes::Resource;
use crate::util::hash::stable_hash;

pub async fn reconcile(
    initial_resources: Vec<Resource>,
    dispatcher: &GeneratorDispatcher,
    context: &Context,
) -> Result<Vec<Resource>> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from(initial_resources);
    let mut output = Vec::new();

    while let Some(resource) = queue.pop_front() {
        // Calculate stable hash for deduplication
        let hash = stable_hash(&resource)?;
        if seen.contains(&hash) {
            tracing::debug!(
                kind = %resource.kind,
                name = %resource.metadata.name,
                "Skipping duplicate resource"
            );
            continue;
        }
        seen.insert(hash);

        // Check if any generator can handle this resource
        if dispatcher.can_generate(&resource) {
            tracing::info!(
                kind = %resource.kind,
                name = %resource.metadata.name,
                "Generating resources"
            );

            let generated = dispatcher.generate(resource, context).await?;
            queue.extend(generated);
        } else {
            // Final resource, add to output
            output.push(resource);
        }
    }

    Ok(output)
}
```

## 5. Kubernetes Layer

### 5.1 Generic Resource Type

```rust
// src/kubernetes/resource.rs

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,

    #[serde(flatten)]
    pub data: BTreeMap<String, serde_norway::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}
```

## 6. Utility Modules

### 6.1 Stable Hashing

```rust
// src/util/hash.rs

use sha2::{Sha256, Digest};
use crate::kubernetes::Resource;

pub fn stable_hash(resource: &Resource) -> Result<String> {
    // Serialize to canonical YAML
    let yaml = serde_norway::to_string(resource)?;

    // Hash with SHA256
    let mut hasher = Sha256::new();
    hasher.update(yaml.as_bytes());
    let hash = hasher.finalize();

    Ok(hex::encode(hash))
}
```

This covers the essential core components. The implementation focuses on clarity, performance, and extensibility.
