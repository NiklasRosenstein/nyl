# ApplicationGenerator Resource Reference

For detailed ApplicationGenerator documentation, see:

**[ArgoCD ApplicationGenerator Guide](../../argocd/application-generator.md)**

## Quick Reference

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: cluster-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/org/repo.git
    targetRevision: HEAD
    path: clusters/default       # Exactly one of path or paths is required
    # paths: ["clusters/*/apps", "shared/apps/*.yaml"]
  project: default
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
  releaseCustomization:
    allowedPaths:
      - metadata.annotations."pref.argocd.argoproj.io/*"
      - spec.info.**
      - spec.ignoreDifferences.**
      - spec.syncPolicy.**
    deniedPaths:
      - spec.syncPolicy.automated.prune
```

The ApplicationGenerator resource enables automatic generation of ArgoCD Applications from NylRelease files in a directory.

Key behavior:
- `source.path` and `source.paths` are mutually exclusive.
- A directory selector is scanned non-recursively by default.
- Use glob selectors in `path`/`paths` when you want recursive discovery.
- `include`/`exclude` patterns are matched against file paths relative to the repository root.
- `NylRelease.spec.argocd.applicationOverride` is always evaluated against `allowedPaths`/`deniedPaths`.
- `allowedPaths`/`deniedPaths` use dotted globs where `*` matches one segment and `**` matches multiple segments.
- If both allow and deny match, deny wins. Ignored fields are reported in generated `Application.spec.info`.

See the [full guide](../../argocd/application-generator.md) for complete field reference, examples, and usage patterns.
