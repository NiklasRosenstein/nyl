# Introduction

**nyl** (pronounced like "nil") is a Kubernetes manifest generator with Helm integration, designed to simplify the management of Kubernetes resources through a powerful templating system.

## Goals

nyl aims to provide:

- **High performance** manifest generation
- **Lightweight binary** for easy distribution
- **Clean, maintainable architecture** for future development
- **Powerful templating** with Helm integration

## Features

- Configuration loading (YAML, JSON)
- File discovery with upward directory traversal
- `nyl validate` command with strict mode
- `nyl new project` command for project scaffolding
- `nyl new component` command for component scaffolding
- Helm integration and component discovery
- Template rendering with Jinja2
- Kubernetes operations (diff, apply)
- Release state management with revision tracking

## Key Features

- **Safety**: Memory safety without garbage collection
- **Performance**: Compiled binaries with minimal runtime overhead
- **Concurrency**: Efficient concurrent operations
- **Reliability**: Strong type system catches errors at compile time
- **Rich ecosystem**: Extensive library support

## Architecture

nyl is structured into several key modules:

- `config`: Project configuration loading and validation
- `cli`: Command-line interface and argument parsing
- `template`: Jinja2 template rendering (Phase 3)
- `kubernetes`: Kubernetes client integration (Phase 4)
- `resources`: Resource definitions and transformations
- `generator`: Manifest generation pipeline (Phase 3+)
