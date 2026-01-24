# Getting Started

This guide will help you get started with nyl-rs.

## Installation

### From Source

```bash
cd nyl-rs
cargo build --release
```

The binary will be available at `target/release/nyl`.

### Install Locally

```bash
cargo install --path .
```

This installs `nyl` to `~/.cargo/bin/nyl`.

## Quick Start

### 1. Create a New Project

```bash
nyl new project my-app
cd my-app
```

This creates:
- `nyl-project.yaml` - Project configuration
- `components/` - Directory for components

### 2. Add a Component

```bash
nyl new component v1.example.io MyApp
```

This creates a new component at `components/v1.example.io/MyApp/` with:
- `Chart.yaml` - Helm chart metadata
- `values.yaml` - Default values
- `values.schema.json` - JSON schema for validation
- `templates/deployment.yaml` - Kubernetes deployment template

### 3. Validate Your Project

```bash
nyl validate
```

Output:
```
✓ Found project config: /path/to/my-app/nyl-project.yaml
✓ Components directory exists: /path/to/my-app/components
✓ Search path exists: /path/to/my-app

✓ Validation passed
```

### 4. Strict Validation

For CI/CD pipelines, use strict mode to treat warnings as errors:

```bash
nyl validate --strict
```

## Project Structure

```
my-app/
├── nyl-project.yaml          # Project configuration
├── components/               # Component definitions
│   └── v1.example.io/
│       └── MyApp/
│           ├── Chart.yaml
│           ├── values.yaml
│           ├── values.schema.json
│           └── templates/
│               └── deployment.yaml
└── lib/                      # Optional: shared libraries (Phase 2+)
```

## Next Steps

- Read about [Configuration](./configuration.md)
- Learn about the [`new` command](./commands/new.md)
- Learn about the [`validate` command](./commands/validate.md)
- Check the [Migration Guide](./migration.md) if you're coming from Python nyl
