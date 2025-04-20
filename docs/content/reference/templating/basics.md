# Basics

Nyl uses [structured-templates](https://pypi.org/project/structured-templates/) to process Kubernetes manifests
before they are applied. This allows you to write expressions in YAML values in the form of `${{ ... }}`, as well as
some form of control flow using special keys like `if(...):` and `for(...)`.

However, this is distinctly different from Helm templates which permit free-form generation of text, which later must
be valid YAML.

## Basic Expressions

Basic expressions allow you to evaluate simple expressions in the form of `${{ ... }}`. This can be used to compute
or retrieve a value, rather than statically typing it in the manifest. The expression result may be of any valid
YAML type, not just strings.

Currently, only the following names are available in the scope of the expression:

- `secrets`: A reference to the secrets provider. This is used to retrieve secrets from the configured secrets
  provider. See [Secrets](./secrets.md) for more information.
- `values`: A container for values defined on the profile-level (see [Profiles](../configuration/profiles.md)) or in the
  manifest (see [Local Variables](./locals.md)). (Note that the use of `locals` instead of `values` is deprecated and
  will be removed in Nyl 0.10.x).
