# CLI Design - Command Structure

## Overview

The Rust rewrite improves CLI UX by splitting the monolithic `template` command into three focused commands following Unix philosophy: `render`, `diff`, and `apply`.

## Command Philosophy

### Design Principles

1. **Single Responsibility**: Each command does one thing well
2. **Composability**: Commands can be piped and combined
3. **Predictable Behavior**: Output goes to stdout by default
4. **Explicit Actions**: `diff` and `apply` are separate, preventing accidental changes

### Comparison with Python Version

**Python (Old)**:
```bash
nyl template manifests/*.yaml                    # render to stdout
nyl template manifests/*.yaml --diff             # show diff
nyl template manifests/*.yaml --apply            # apply to cluster
```

**Rust (New)**:
```bash
nyl render manifests/*.yaml                      # render to stdout
nyl diff manifests/*.yaml                        # show diff
nyl apply manifests/*.yaml                       # apply to cluster
```

**Benefits**:
- ✅ Clearer intent - command name shows what will happen
- ✅ Harder to accidentally apply changes (no `--apply` flag to miss)
- ✅ Better for scripting (predictable output destinations)
- ✅ Follows kubectl's command structure (familiar to users)

## Core Commands

### `nyl render`

**Purpose**: Render templates and output Kubernetes manifests to stdout

**Usage**:
```bash
nyl render [OPTIONS] <FILES>...
```

**Options**:
- `-C, --project-dir <DIR>` - Project directory (default: current)
- `-s, --set <KEY=VALUE>` - Set values (repeatable)
- `-f, --values <FILE>` - Values file (YAML, repeatable)
- `-o, --output <FILE>` - Output to file instead of stdout
- `--pretty` - Pretty print YAML (multi-line)

**Examples**:
```bash
# Render to stdout
nyl render manifests/*.yaml

# Render with values
nyl render manifests/*.yaml -s env=prod -s region=us-west-2

# Render with values file
nyl render manifests/*.yaml -f values/prod.yaml

# Render to file
nyl render manifests/*.yaml -o output.yaml

# Pipe to kubectl
nyl render manifests/*.yaml | kubectl apply -f -

# Save for review
nyl render manifests/*.yaml > review.yaml
git diff review.yaml
```

**Exit Codes**:
- `0` - Success
- `1` - Rendering error (template, YAML, Helm)
- `2` - Configuration error
- `3` - File not found

---

### `nyl diff`

**Purpose**: Show diff of rendered manifests against cluster state

**Usage**:
```bash
nyl diff [OPTIONS] <FILES>...
```

**Options**:
- `-C, --project-dir <DIR>` - Project directory
- `-s, --set <KEY=VALUE>` - Set values (repeatable)
- `-f, --values <FILE>` - Values file (YAML, repeatable)
- `-c, --context <NUM>` - Context lines around changes (default: 3)
- `--server-side` - Server-side diff (requires kubectl 1.28+)

**Examples**:
```bash
# Show diff against cluster
nyl diff manifests/*.yaml

# Show diff with custom context
nyl diff manifests/*.yaml -c 5

# Check what would change in production
nyl diff manifests/*.yaml -f values/prod.yaml
```

**How it Works**:
1. Render manifests using same logic as `nyl render`
2. Pipe rendered YAML to `kubectl diff -f -`
3. Display kubectl's diff output
4. Exit with kubectl's exit code

**Exit Codes**:
- `0` - No differences
- `1` - Differences found (normal diff output)
- `2` - Error (rendering error, cluster unreachable, etc.)

**Notes**:
- Requires `kubectl` in PATH
- Requires valid kubeconfig and cluster access
- Uses kubectl's diff format (similar to `git diff`)

---

### `nyl apply`

**Purpose**: Apply rendered manifests to Kubernetes cluster

**Usage**:
```bash
nyl apply [OPTIONS] <FILES>...
```

**Options**:
- `-C, --project-dir <DIR>` - Project directory
- `-s, --set <KEY=VALUE>` - Set values (repeatable)
- `-f, --values <FILE>` - Values file (YAML, repeatable)
- `--dry-run` - Client-side dry run (no changes)
- `--wait` - Wait for resources to be ready
- `--timeout <SECONDS>` - Timeout for wait (default: 300)
- `--prune` - Delete resources not in input (future feature)

**Examples**:
```bash
# Apply to cluster
nyl apply manifests/*.yaml

# Dry run first
nyl apply manifests/*.yaml --dry-run

# Apply and wait for readiness
nyl apply manifests/*.yaml --wait --timeout 600

# Apply production config
nyl apply manifests/*.yaml -f values/prod.yaml
```

**How it Works**:
1. Render manifests
2. Pipe to `kubectl apply -f -`
3. Show kubectl output
4. Exit with kubectl's exit code

**Exit Codes**:
- `0` - Successfully applied
- `1` - Apply failed (validation, permissions, cluster error)
- `2` - Rendering error

**Safety Features**:
- Separate command prevents accidental applies
- Dry run available for safety checks
- Works with standard kubectl flags via passthrough (future)

---

### `nyl new`

**Purpose**: Create a new Nyl project with scaffolding

**Usage**:
```bash
nyl new [OPTIONS] <NAME>
```

**Options**:
- `-p, --path <DIR>` - Project path (default: `./<name>`)
- `--with-examples` - Include example manifests

**Examples**:
```bash
# Create new project
nyl new my-app

# Create with examples
nyl new my-app --with-examples

# Create in specific directory
nyl new my-app --path /path/to/projects/my-app
```

**Generated Structure**:
```
my-app/
├── nyl-project.yaml
├── components/
└── manifests/
    └── example.yaml (if --with-examples)
```

---

### `nyl validate`

**Purpose**: Validate configuration files without rendering

**Usage**:
```bash
nyl validate [OPTIONS] [FILES]...
```

**Options**:
- `-C, --project-dir <DIR>` - Project directory

**Examples**:
```bash
# Validate project config
nyl validate

# Validate specific files
nyl validate manifests/*.yaml

# Validate with specific project dir
nyl validate -C /path/to/project
```

**Validation Checks**:
- ✅ YAML syntax
- ✅ Configuration schema
- ✅ Required fields present
- ✅ Type correctness
- ⚠️ Unknown fields (warning)
- ⚠️ Deprecated options (warning)

**Exit Codes**:
- `0` - Valid
- `1` - Validation errors
- `2` - File not found

---

## Global Flags

Available on all commands:

```bash
-h, --help          Show help
-V, --version       Show version
-v, --verbose       Enable verbose logging
--log-level <LEVEL> Set log level (error, warn, info, debug, trace)
```

**Examples**:
```bash
# Verbose output for debugging
nyl render manifests/*.yaml -v

# Debug level logging
nyl render manifests/*.yaml --log-level debug

# Show version
nyl --version
```

## Command Workflows

### Development Workflow

```bash
# 1. Create new project
nyl new my-app --with-examples
cd my-app

# 2. Edit manifests
vim manifests/deployment.yaml

# 3. Validate
nyl validate

# 4. Preview rendered output
nyl render manifests/*.yaml

# 5. Check diff (against dev cluster)
nyl diff manifests/*.yaml -f values/dev.yaml

# 6. Apply to dev
nyl apply manifests/*.yaml -f values/dev.yaml
```

### Production Deployment Workflow

```bash
# 1. Review changes
nyl render manifests/*.yaml -f values/prod.yaml > /tmp/prod-manifests.yaml
git diff /tmp/prod-manifests.yaml

# 2. Diff against production cluster
export KUBECONFIG=~/.kube/prod-config
nyl diff manifests/*.yaml -f values/prod.yaml

# 3. Get approval
# ... manual review process ...

# 4. Apply to production
nyl apply manifests/*.yaml -f values/prod.yaml --wait
```

### CI/CD Pipeline

```bash
# In CI pipeline
set -e

# Validate
nyl validate

# Render (verify no errors)
nyl render manifests/*.yaml -f values/${ENV}.yaml > /dev/null

# Diff (for PR comments)
nyl diff manifests/*.yaml -f values/${ENV}.yaml || true

# Apply (in CD)
if [ "$CI_COMMIT_BRANCH" = "main" ]; then
  nyl apply manifests/*.yaml -f values/${ENV}.yaml --wait
fi
```

## Future Commands

### `nyl get` (v0.2.0+)

Get resources from cluster or local state:

```bash
nyl get helmcharts
nyl get components
```

### `nyl template` (v0.3.0+)

Template arbitrary files (not just K8s manifests):

```bash
nyl template template.txt -s name=value
```

### `nyl component` (v0.4.0+)

Manage components:

```bash
nyl component list
nyl component create my-component
```

## Advantages Over Python Version

1. **Safety**: Explicit commands prevent accidental applies
2. **Unix Philosophy**: Each command does one thing well
3. **Composability**: Easy to pipe and chain
4. **Discoverability**: Command names self-document
5. **Consistency**: Follows kubectl pattern
6. **Scripting**: Predictable behavior for automation

## Migration from Python

### Command Translation

| Python | Rust | Notes |
|--------|------|-------|
| `nyl template` | `nyl render` | Default behavior |
| `nyl template --diff` | `nyl diff` | Separate command |
| `nyl template --apply` | `nyl apply` | Separate command |
| `nyl new` | `nyl new` | Unchanged |
| (none) | `nyl validate` | New command |

### Scripts to Update

**Before (Python)**:
```bash
nyl template manifests/*.yaml --apply
```

**After (Rust)**:
```bash
nyl apply manifests/*.yaml
```

**Or with review**:
```bash
nyl diff manifests/*.yaml
nyl apply manifests/*.yaml
```
