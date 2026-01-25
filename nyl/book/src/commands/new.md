# new

Create new nyl projects and components.

## Synopsis

```bash
nyl new project <name> [--path <path>]
nyl new component <api-version> <kind>

# Legacy syntax (deprecated)
nyl new <name> [--path <path>]
```

## Subcommands

### `project`

Create a new nyl project with default structure.

**Arguments:**
- `<name>` - Name of the project

**Options:**
- `--path <path>` - Path where to create the project (default: current directory)

**Example:**

```bash
nyl new project my-app
cd my-app
```

Creates:
```
my-app/
├── nyl-project.yaml
└── components/
```

**Default Configuration:**

The created `nyl-project.yaml` contains:

```yaml
settings:
  generate_applysets: false
  on_lookup_failure: Error
  components_path: components
  search_path:
    - .
```

### `component`

Create a new component in the current project.

**Arguments:**
- `<api-version>` - Component API version (e.g., `v1.example.io`)
- `<kind>` - Component kind (e.g., `MyApp`)

**Example:**

```bash
nyl new component v1.example.io MyApp
```

Creates:
```
components/v1.example.io/MyApp/
├── Chart.yaml
├── values.yaml
├── values.schema.json
└── templates/
    └── deployment.yaml
```

**Files Created:**

- `Chart.yaml` - Helm chart metadata
- `values.yaml` - Default values for the component
- `values.schema.json` - JSON schema for value validation
- `templates/deployment.yaml` - Basic Kubernetes deployment template

## Legacy Syntax

The legacy syntax `nyl new <name>` is deprecated but still supported for backward compatibility:

```bash
nyl new my-app
```

This will show a deprecation warning and create a project. Use `nyl new project <name>` instead.

## Examples

### Create a project in a specific directory

```bash
nyl new project my-app --path ~/projects
cd ~/projects/my-app
```

### Create a complete project with component

```bash
# Create project
nyl new project my-app
cd my-app

# Create component
nyl new component v1.example.io WebServer

# Validate
nyl validate
```

## Component Template Structure

The generated component follows Helm chart conventions:

### Chart.yaml

```yaml
apiVersion: v2
name: myapp
description: A Helm chart for MyApp
type: application
version: 0.1.0
appVersion: "1.0"
```

### values.yaml

```yaml
replicaCount: 1

image:
  repository: nginx
  pullPolicy: IfNotPresent
  tag: "latest"

service:
  type: ClusterIP
  port: 80

resources:
  limits:
    cpu: 100m
    memory: 128Mi
  requests:
    cpu: 100m
    memory: 128Mi
```

### templates/deployment.yaml

A basic Kubernetes Deployment template using Helm templating syntax.

## Error Handling

### Project already exists

```bash
$ nyl new project existing-app
Error: Project directory already exists: ./existing-app
```

### Component already exists

```bash
$ nyl new component v1.example.io MyApp
Error: Component already exists: ./components/v1.example.io/MyApp
```

### No project configuration found

If you try to create a component without a project configuration:

```bash
$ nyl new component v1.example.io MyApp
# Creates component in ./components/ (default)
```
