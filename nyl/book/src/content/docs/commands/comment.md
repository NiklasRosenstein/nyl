---
title: 'comment'
description: 'Maintain a sticky Markdown comment on GitHub, GitLab, or Forgejo.'
---

`nyl comment upsert` maintains one comment per stable key and authenticated
account on a pull request or merge request. It works without a Nyl project and
accepts Markdown from any report generator.

```bash
nyl comment upsert --key gitops/kasoku --body-file report.md
cat report.md | nyl comment upsert --key gitops/kasoku --body-file -
```

## Options and context

| Option | Meaning |
| --- | --- |
| `--key KEY` | Required stable key, 1–256 UTF-8 bytes; cannot be blank. |
| `--body-file PATH` | Required UTF-8 Markdown file; `-` reads stdin. Relative paths use the invocation directory. |
| `--provider github\|gitlab\|forgejo` | Overrides CI provider detection. |
| `--server-url URL` | Overrides the forge **web** URL, including any installation path prefix. |
| `--repository REPOSITORY` | Overrides `owner/repo`, a GitLab `group/subgroup/project` path, or a numeric GitLab project ID. |
| `--request NUMBER` | Overrides the positive PR number or project-local MR IID. |
| `--dry-run` | Authenticates and reads remote state, prints the proposed outcome, and performs no comment writes. |

Explicit coordinates take precedence over CI context. No Git remote or project
configuration is required. Self-hosted instances use their standard API paths:
GitHub Enterprise Server `/api/v3` and `/api/graphql`, GitLab `/api/v4`, and
Forgejo `/api/v1`. Supply a web URL, such as `https://git.example.com/forge`,
without credentials, query parameters, or an API suffix. Redirects are disabled;
use the canonical instance URL.

```bash
nyl comment upsert --provider forgejo \
  --server-url https://git.example.com/forge \
  --repository platform/config --request 42 \
  --key gitops/kasoku --body-file report.md --dry-run
```

CI detection uses:

- **GitHub Actions:** `GITHUB_ACTIONS`, `GITHUB_SERVER_URL`,
  `GITHUB_REPOSITORY`, and `GITHUB_EVENT_PATH`. The event's pull request base
  repository takes precedence over `GITHUB_REPOSITORY`, including fork PRs.
  PR events and PR `issue_comment` events supply the number; `GITHUB_REF` with
  `refs/pull/NUMBER/head` or `refs/pull/NUMBER/merge` is a fallback.
  The default server is `https://github.com`.
- **GitLab CI:** `GITLAB_CI`, `CI_SERVER_URL`,
  `CI_MERGE_REQUEST_PROJECT_ID` (then `CI_PROJECT_PATH`, then `CI_PROJECT_ID`),
  and `CI_MERGE_REQUEST_IID`. The MR's project ID identifies its owning project
  in fork pipelines. The default server is `https://gitlab.com`.
- **Forgejo Actions:** `FORGEJO_ACTIONS` or `GITEA_ACTIONS` takes precedence over
  GitHub detection. `FORGEJO_SERVER_URL` also identifies Forgejo. Server URL,
  repository, event path, and ref use `FORGEJO_*`, then `GITEA_*`, then the
  compatible `GITHUB_*` variables. Event interpretation matches GitHub's.
  Forgejo requires a detected or explicit server URL.

Push and scheduled jobs usually need `--request`. Keep credentials scoped to the
selected instance and repository when overriding CI coordinates.

## Credentials and minimum permissions

Tokens are read only from environment variables. Nyl does not accept token
arguments, print credentials, or include remote response bodies in errors.

| Provider | Environment variables, in priority order | Permissions |
| --- | --- | --- |
| GitHub | `GH_TOKEN`, `GITHUB_TOKEN` | Repository **Pull requests: write** or **Issues: write**. GitHub Actions can use `permissions: { pull-requests: write }`. Fine-grained PATs and App installation tokens must include the repository. Classic PATs need `public_repo` for public repositories or `repo` for private repositories. |
| GitLab | `GITLAB_TOKEN` | Personal, project, or group access token with `api` scope, and a role allowed to read and comment on the MR: Planner on versions supporting MR comments for that role, or Reporter. |
| Forgejo | `FORGEJO_TOKEN` | Token with `read:user` for account identity and `write:issue` for comments, plus access to the repository. Use an account token supporting these scopes; repository-specific tokens that cannot call `/user` are insufficient. |

GitHub account identity comes from authenticated GraphQL `viewer`, which also
supports GitHub Actions and App installation tokens. GitLab and Forgejo use
their authenticated `/user` endpoints. Nyl does not infer ownership from the CI
actor or adopt another account's comments. Keep the same account across runs;
rotating its token preserves ownership.

`CI_JOB_TOKEN` cannot create GitLab notes. Forgejo's compatible `GITHUB_TOKEN`
variable is not used as a credential fallback; export the selected credential as
`FORGEJO_TOKEN`. Fork pipelines may lack secrets or have read-only tokens. Run the
posting step only where the token has permission, and do not execute untrusted
PR code in a privileged posting job.

See the provider references for [GitHub comment permissions](https://docs.github.com/en/rest/issues/comments),
[GitLab notes](https://docs.gitlab.com/api/notes/),
[GitLab MR roles](https://docs.gitlab.com/user/permissions/#project-merge-requests),
[GitLab job token restrictions](https://docs.gitlab.com/ci/jobs/ci_job_token/),
and [Forgejo token scopes](https://forgejo.org/docs/latest/user/authentication/token-scope/).

## Outcomes and recovery

Successful runs print one line on stdout and exit zero:

```text
created https://github.com/owner/repo/pull/42#issuecomment-123
updated https://github.com/owner/repo/pull/42#issuecomment-123
unchanged https://github.com/owner/repo/pull/42#issuecomment-123
```

Dry runs prefix the outcome with `dry-run`. A proposed creation prints the PR/MR
URL because a comment URL does not exist yet. Errors go to stderr and exit
nonzero.

The first line is a hidden marker:
`<!-- nyl-comment:v1:HEX_UTF8_KEY -->`. Nyl hex-encodes the key, adds a blank
line, and preserves the supplied Markdown bytes. Do not include a `nyl-comment`
marker in the report itself. Different keys coexist. A matching marker must be
the first line of a comment authored by the authenticated account. Comments from
other accounts, other marker versions, and GitLab system notes are ignored.

Nyl scans every comment page before deciding what to write, including instances
that cap page sizes below the requested size. Identical complete bodies perform
no write. Multiple owned comments with the same marker are an error: remove the
duplicates manually and rerun.

The complete payload, including the marker, is limited by Nyl to 65,536 UTF-8
bytes on GitHub and Forgejo, and 1,000,000 bytes on GitLab. These conservative
byte limits can be stricter than a server's character limit. Instance settings
may impose smaller limits. Nyl reports oversized content without truncating it;
shorten the report and link to a CI artifact.

Requests have a 30-second timeout. If a create response is lost, malformed, or
indicates an uncertain server failure, Nyl searches again before making at most
one create retry. A discovered comment is reconciled and may report `unchanged`.
A failed recheck stops the operation. Authentication, permission, and size
rejections are not retried.

Forge comment APIs do not provide an atomic upsert by marker. Serialize jobs
for the same repository, request, account, and key to avoid concurrent creates
and out-of-order reports. Recovery limits duplicates but cannot guarantee their
absence when writes remain invisible during the recheck.

## CI examples

`diff-tree --stats-output markdown:PATH` produces a combined diff and validation
report suitable for `--body-file`. It includes validation failures and available
comparison results even when the command exits unsuccessfully. Capture that
status, post the report, then return the captured failure. See the
[combined-report CI example](/nyl/commands/gitops/#markdown-prmr-comments) for the
complete shell flow and artifact handling.

Reports have a 60,000-byte Markdown budget, with explicit truncation notices for
large findings, file tables, and optional `--stats-patch` previews. Supply
`--stats-artifacts-url` to link to complete CI artifacts. This budget leaves room
for the sticky marker; `comment upsert` still validates the final body size.

For a GitHub Actions job with Nyl installed and `report.md` available:

```yaml
permissions:
  pull-requests: write
concurrency:
  group: comment-${{ github.repository }}-${{ github.event.pull_request.number }}-gitops-kasoku
  cancel-in-progress: false
steps:
  - name: Post report
    env:
      GITHUB_TOKEN: ${{ github.token }}
    run: nyl comment upsert --key gitops/kasoku --body-file report.md
```

For a GitLab MR job, provide `GITLAB_TOKEN` as a masked CI variable and make the
report available through the job's artifacts:

```yaml
comment:
  rules:
    - if: '$CI_PIPELINE_SOURCE == "merge_request_event"'
  resource_group: comment-$CI_MERGE_REQUEST_IID-gitops-kasoku
  script:
    - nyl comment upsert --key gitops/kasoku --body-file report.md
```

For Forgejo Actions, supply an account token as the `COMMENT_TOKEN` secret:

```yaml
steps:
  - name: Post report
    env:
      FORGEJO_TOKEN: ${{ secrets.COMMENT_TOKEN }}
    run: nyl comment upsert --key gitops/kasoku --body-file report.md
```
