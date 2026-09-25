# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Syntax for late-bound field templates

A templated control resource is rendered as a whole before its fields are
used, so a field that is itself a template rendered later with extra context
competes with that first pass. Verified with `render-tree`:
`applicationNameTemplate: '{{ target.metadata.name }}-{{ release.metadata.name }}'`
fails, because `release` is undefined in the first pass; wrapping it in
`{% raw %}…{% endraw %}` renders `production-api`. Today's validation hint
suggests the failing form (a separate fix is queued as a task).

Options: keep one syntax and document `{% raw %}`; give late-bound field
templates their own delimiters that cannot collide with the structural pass,
such as `${ release.metadata.name }` (MiniJinja supports custom delimiters),
while still accepting a Jinja template that survives the first pass, for
compatibility; or replace such fields with structured alternatives, such as a
name prefix. Units will add more late-bound contexts, so the choice applies
beyond ApplicationGroups.

## Next

### 2. Scenario coverage by milestone

- M3's Command stand-ins cannot express `bind: plan` approvals or teardown
  steps, so the platform scenario's approval and teardown steps cannot pass in
  M3.
- M4's slice of the platform scenario includes the publication unit, an M5
  item.
- Tier 2 skips when tools are missing, so "passes with the real tools" can pass
  without running; decide where tier 2 runs as a required check.

## Later

- Hotfixes for environments whose source is promoted, for example a second
  source branch such as `release/prod` with its own environment and promotion
  path into prod, once promotion sources are settled.

- Promotion scenario (M6): staging or prod promoted from dev, extending the
  platform scenario.
