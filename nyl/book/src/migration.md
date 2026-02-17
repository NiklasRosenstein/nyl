# Migration from Python

## Breaking Configuration Change

nyl now supports project settings only from `nyl.toml`.

Legacy project files are not loaded:
- `nyl.toml`
- `nyl.toml`
- `nyl.toml`
- `.nyl-project.*`

## New Project Config Format

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
```

## Convert Existing Projects

1. Create `nyl.toml` at project root.
2. Move path settings into `[project]`.
3. Move profile settings to `nyl-profiles.yaml` if needed.
4. Run:

```bash
nyl validate --strict
```

## Schema Support

Generate current schema:

```bash
nyl generate schema config
```
