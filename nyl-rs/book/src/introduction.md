# Introduction

**nyl** (pronounced "nile") is a Kubernetes manifest generator with Helm integration, designed to simplify the management of Kubernetes resources through a powerful templating system.

This is the Rust rewrite of the Python-based nyl tool, focusing on performance and clean architecture.

## Goals

The Rust rewrite aims to achieve:

- **5-10x performance improvement** over the Python implementation
- **Binary size under 20MB** for easy distribution
- **100% compatibility** with existing nyl projects
- **Clean, maintainable architecture** for future development

## Current Status

### Phase 1: Configuration & CLI Foundation ✅ COMPLETE

- Configuration loading (YAML, JSON)
- File discovery with upward directory traversal
- `nyl validate` command with strict mode
- `nyl new project` command for project scaffolding
- `nyl new component` command for component scaffolding
- Comprehensive test coverage

### Coming Soon

- **Phase 2**: Helm integration and component discovery
- **Phase 3**: Template rendering with Jinja2
- **Phase 4**: Kubernetes operations (diff, apply)

## Why Rust?

Rust provides:

- **Safety**: Memory safety without garbage collection
- **Performance**: Compiled binaries with minimal runtime overhead
- **Concurrency**: Fearless concurrent operations
- **Reliability**: Strong type system catches errors at compile time
- **Ecosystem**: Rich library ecosystem with cargo

## Architecture

nyl-rs is structured into several key modules:

- `config`: Project configuration loading and validation
- `cli`: Command-line interface and argument parsing
- `template`: Jinja2 template rendering (Phase 3)
- `kubernetes`: Kubernetes client integration (Phase 4)
- `resources`: Resource definitions and transformations
- `generator`: Manifest generation pipeline (Phase 3+)
