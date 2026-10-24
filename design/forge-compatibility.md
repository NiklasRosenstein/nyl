# Git host compatibility of orchestration state

Orchestration state relies on two things a Git host must accept (see
[Environment](orchestration-core.md#environment) and
[Runs and leases](orchestration-core.md#runs-and-leases)):

- refs under the coordination prefix, by default the custom namespace
  `refs/nyl/`, created and deleted on every run, and listed by pattern;
- atomic pushes that move a state branch and a coordination ref together,
  each under its own lease (`git push --atomic --force-with-lease=<ref>:<expected>`),
  where one stale lease rejects the whole push.

Nyl's own use of these is covered by the state store conformance tests
against local repositories. This note records which hosts accept them.

## Results

Verified on 2026-10-24 with both the default prefix `refs/nyl/` and the
branch prefix `refs/heads/nyl-run/`:

| Host | Version | `refs/nyl/` | `refs/heads/nyl-run/` |
|---|---|---|---|
| GitHub (github.com), SSH | hosted | pass | pass |
| GitLab (gitlab.com), SSH | hosted | pass | pass |
| Forgejo, HTTP | 11.0.6 | pass | pass |
| Plain Git, `file://` | git 2.43 | pass | pass |

Hosts that reject custom refs, or refs outside `refs/heads/` and
`refs/tags/`, work with `coordinationRefPrefix: refs/heads/nyl-run/`. Hosts
without atomic push support cannot hold orchestration state: separate desired
and observed refs and the lease must change together.

## Checks

Each check ran against a writable repository, on refs named with a random
suffix that the last check deletes again. With `B` a branch such as
`refs/heads/nyl-check/<suffix>/desired`, `L` the lease `<prefix>nyl-check-<suffix>/lease`,
and commits `s1`, `l1`, `s2`, `l2` where `s2` and `l2` descend from `s1` and `l1`:

1. **Create together.** `git push --atomic --force-with-lease=B: --force-with-lease=L: <url> s1:B l1:L`
   creates both refs.
2. **Reject a stale lease.** `git push --atomic --force-with-lease=B:s1 --force-with-lease=L:s1 <url> s2:B l2:L`
   fails, and neither ref moves.
3. **Advance together.** `git push --atomic --force-with-lease=B:s1 --force-with-lease=L:l1 <url> s2:B l2:L`
   moves both refs.
4. **List by pattern.** `git ls-remote <url> '<prefix>nyl-check-<suffix>/*'`
   lists `L`.
5. **Delete together.** `git push --atomic --force-with-lease=L:l2 --force-with-lease=B:s2 <url> :L :B`
   deletes both refs.

To verify another host, run the same pushes against a scratch repository on
it. Git compares a lease only on refs a push changes, so every check moves
each leased ref to a new commit or deletes it.
