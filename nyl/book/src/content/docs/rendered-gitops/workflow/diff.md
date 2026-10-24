---
title: 'Diff in Pull Requests'
---

Compare the desired tree with the currently published deployment revision:

```bash
nyl diff-tree \
  --target production \
  --against published \
  --output rendered.diff
```

Compare instead with the tree generated from the source default branch:

```bash
nyl diff-tree \
  --target production \
  --against source \
  --source-ref main \
  --output rendered.diff
```

The source-derived form remains accurate when a publication job has not yet
updated the deployment revision. `--fail-on-diff` returns a non-zero status
after writing the complete diff. Comparison coordinates and human status are
written to stderr, including the desired source commit and the resolved
published or source baseline commit. Stdout and `--output` contain only unified
diff bytes; no differences produce zero bytes.

YAML comparisons normalize mapping order and presentation, so formatting and
comment changes do not affect the patch, statistics, or `--fail-on-diff`.
Use `--raw` for byte comparisons and text patches applicable to stored files.
See the [comparison rules](/nyl/reference/cli/diff-tree/) for details.

Use `--catalog` for generated Applications and AppProjects, `--applications`
for every workload Application and payload, or repeat
`--application <namespace>/<name>` for a focused review. Application views are
derived from each generated Application's plain-directory source path, so they
follow customized rendered layouts without changing `_nyl/index.json`.

[`nyl comment upsert`](/nyl/reference/cli/comment/) posts a Markdown report to a pull
or merge request on GitHub, GitLab, or Forgejo. Use a stable `--key` to update
the same authenticated account's comment on later pipelines. Report generation
and posting are separate commands.


See [`diff-tree`](/nyl/reference/cli/diff-tree/) for all flags.
