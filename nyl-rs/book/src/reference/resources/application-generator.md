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
    path: clusters/default
  project: default
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
```

The ApplicationGenerator resource enables automatic generation of ArgoCD Applications from NylRelease files in a directory.

See the [full guide](../../argocd/application-generator.md) for complete field reference, examples, and usage patterns.
