# render

> **Status**: Phase 3 (Not yet implemented)

Render Kubernetes manifests from nyl components and templates.

## Synopsis

```bash
nyl render [options]
```

## Description

The `render` command will generate Kubernetes manifests by:

1. Loading project configuration
2. Discovering components
3. Rendering templates with Jinja2
4. Processing Helm charts
5. Outputting YAML manifests to stdout

This command will be implemented in Phase 3.

## Planned Features

- Template rendering with Jinja2
- Helm chart integration
- Component discovery and loading
- Profile support for different environments
- Secret provider integration
- Applyset generation

## Coming Soon

This command is planned for Phase 3 of the Rust rewrite.
