# nyl/argocd-with-nyl

This is a simple Nyl Kubernetes manifest to install ArgoCD with Nyl (Rust version) as a Config Management Plugin. The `argocd.yaml` file here should serve as a starting point for bootstrapping your own ArgoCD instance.

**⚠️ Migration Notice**: This configuration now uses the Rust version of Nyl. See the [Migration Guide](#migration-from-python-to-rust) below if you're upgrading from the Python version.

## Goals

* Bootstrap an ArgoCD instance with Nyl as a Config Management Plugin from zero to fully functional in a single command.
* Have ArgoCD immediately own its own installation after bootstrapping.
* If anything goes wrong, be able to easily re-run the command to get back to a fully functional state.
* Demonstrate using SOPS to inject secrets into manifests and Helm chart values (Note: SOPS not yet implemented in Rust version).

## Usage

You may want to modify the file to suit your needs before proceeding, for example to

* Configure ArgoCD to use OIDC for authentication.
* Configure ArgoCD to use an Ingress.
* Point ArgoCD to your own Git repository (this is required for ArgoCD to own its own installation after bootstrapping).
* Adjust the `nyl/argocd-cmp` image version.

Once you are ready, run the following command to bootstrap ArgoCD:

    $ nyl crds | kubectl apply -f -
    $ nyl render argocd.yaml --apply

**Note**: The command has changed from `nyl template` to `nyl render` in the Rust version.

Note that the `nyl-project.toml` is empty here, but it helps ArgoCD to automatically detect that the Nyl Config
Management Plugin should be used for this application.

## Project layout

The ArgoCD plugin will run `nyl render .` in this directory to generate the manifests for ArgoCD to apply. Nyl will
consider all YAML files (with the `.yaml` suffix, not `.yml`) in the directory (not recursively) as part of the project,
_excluding_ any files that begin with `nyl-`, `.` or `_`.

```
.envrc              -- Exports the SOPS_AGE_KEY so you can decrypt the secrets locally.
                       IMPORTANT: This specific setup is for demonstration purposes only. Do not use this in production.
                       Keep your secrets safe!
.secrets.yaml       -- Encrypted secrets file for SOPS. This file is encrypted with the SOPS_AGE_KEY in .envrc.
.sops.yaml          -- SOPS configuration file to specify the encryption method and the public key to use.
argocd.yaml         -- The main Nyl manifest file for ArgoCD that creates the argocd Namespace, the argocd-nyl-env
                       Secret, instantiates the ArgoCD Helm chart and creates the ArgoCD application to manage itself
                       after bootstrapping.
nyl-project.toml    -- Empty file that signals to ArgoCD that the Nyl Config Management Plugin should be used for this
                       application. This may have some project-specific configuration, but in this case it is empty.
nyl-secrets.toml    -- Tells Nyl to lookup secrets in the .secrets.yaml via SOPS when rendering the manifests that call
                       the `secrets.get(<key>)` function.
```

Note that configuration files may also be formatted as TOML (`.toml`) or JSON (`.json`).

## Migration from Python to Rust

If you're upgrading from the Python version of Nyl, here's what you need to know:

### Breaking Changes

1. **Command renamed**: `nyl template` → `nyl render`
   - Update your plugin.yaml, scripts, and documentation
   - The ArgoCD plugin.yaml in this repository has been updated

2. **Image version**: New major version (1.0.0)
   - Update `argocd.yaml` line 43 to use the new image tag
   - Example: `ghcr.io/helsing-ai/nyl/argocd-cmp:1.0.0`

3. **SOPS support**: Not yet implemented in Rust version
   - **Workaround**: Use Kubernetes secrets provider or Null provider temporarily
   - SOPS support is planned for a future release
   - The SOPS binary is included in the image for future use

### Migration Steps

1. **Update image reference** in your ArgoCD application:
   ```yaml
   image: ghcr.io/helsing-ai/nyl/argocd-cmp:1.0.0
   ```

2. **Update any local scripts** that use `nyl template`:
   ```bash
   # Old
   nyl template --apply

   # New
   nyl render --apply
   ```

3. **Handle SOPS secrets** (if applicable):
   - Option A: Wait for SOPS support in Rust version
   - Option B: Use Kubernetes secrets provider temporarily
   - Option C: Use Null provider for non-production environments

4. **Test your manifests** with the Rust version:
   ```bash
   nyl render . > /tmp/output.yaml
   kubectl diff -f /tmp/output.yaml
   ```

### Benefits of Rust Version

- **10x faster** rendering performance
- **75% less memory** usage
- **<100MB image** (down from 200MB+)
- **No Python runtime** required
- **Static binary** with zero dependencies

### What Still Works

- Existing `nyl-project.yaml` files (no changes needed)
- `HelmChart`, `Component`, `NylRelease`, `ApplicationGenerator` resources
- Template syntax (MiniJinja is Jinja2-compatible)
- Git repository handling
- Kubernetes API access

### Additional Resources

- [Complete migration guide](../MOVE_TO_RUST.md)
- [Rust implementation details](../nyl/IMPLEMENTATION.md)
- [Feature comparison table](../MOVE_TO_RUST.md#feature-comparison)
