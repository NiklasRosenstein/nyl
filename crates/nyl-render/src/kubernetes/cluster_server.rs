//! Whether a kubeconfig context reaches the cluster a Cluster declares.
//!
//! A Cluster declares its Argo CD destination as a server URL or a registered
//! name. A context whose server URL differs from a declared concrete URL is
//! another cluster, and applying or reading through it would act on the wrong
//! one, so every entry point that connects through a Cluster checks it. The
//! in-cluster address and a destination by name cannot be compared with a
//! local kubeconfig endpoint and are not checked.

use crate::resources::Cluster;
use crate::{NylError, Result};

const IN_CLUSTER: &str = "https://kubernetes.default.svc";

/// Whether a declared destination server can be compared with a context's
/// server: a concrete URL other than the in-cluster address.
pub fn verifiable_cluster_server(server: &str) -> bool {
    normalize_cluster_url(server) != IN_CLUSTER
}

/// Fail when `config`'s server is not the one `cluster` declares.
pub fn verify_cluster_server(cluster: &Cluster, config: &kube::Config) -> Result<()> {
    let Some(expected) = cluster.spec.destination.server.as_deref() else {
        return Ok(());
    };
    verify_server(&cluster.metadata.name, expected, &config.cluster_url.to_string())
}

/// Fail when `actual`, a context's server, is not `expected`, a declared one.
pub fn verify_server(cluster: &str, expected: &str, actual: &str) -> Result<()> {
    if !verifiable_cluster_server(expected) {
        tracing::debug!(
            cluster = %cluster,
            "Cannot verify an in-cluster Argo CD destination against a local kubeconfig endpoint"
        );
    } else if normalize_cluster_url(expected) != normalize_cluster_url(actual) {
        return Err(NylError::config(format!(
            "Selected kube context points to {actual}, but Cluster '{cluster}' expects {expected}"
        )));
    }
    Ok(())
}

/// A destination server in the form two spellings of one cluster share: no
/// trailing slash, and the scheme and authority in lower case.
pub fn normalize_cluster_url(value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    let Some((scheme, remainder)) = value.split_once("://") else {
        return value.to_owned();
    };
    let boundary = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let (authority, suffix) = remainder.split_at(boundary);
    format!(
        "{}://{}{suffix}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a context for another cluster being accepted, and a spelling
    /// difference of the same server being refused.
    #[test]
    fn test_a_context_must_reach_the_declared_server() {
        assert!(verify_server("dev", "https://Kind.Example:6443/", "https://kind.example:6443").is_ok());
        let error = verify_server("dev", "https://kind.example:6443", "https://prod.example:6443").unwrap_err();
        assert!(
            error.to_string().contains("expects https://kind.example:6443"),
            "{error}"
        );
    }

    /// Catches the in-cluster address being compared with a local endpoint,
    /// and being counted as proof of the cluster.
    #[test]
    fn test_the_in_cluster_address_is_not_verifiable() {
        assert!(!verifiable_cluster_server("https://kubernetes.default.svc"));
        assert!(verify_server("dev", "https://kubernetes.default.svc", "https://127.0.0.1:6443").is_ok());
        assert!(verifiable_cluster_server("https://127.0.0.1:6443"));
    }

    /// Catches case-folding a server's path: only the scheme and authority are
    /// case-insensitive.
    #[test]
    fn test_normalization_preserves_case_sensitive_paths() {
        assert_eq!(
            normalize_cluster_url("HTTPS://EXAMPLE.invalid/clusters/Prod/"),
            "https://example.invalid/clusters/Prod"
        );
        assert_ne!(
            normalize_cluster_url("https://example.invalid/clusters/Prod"),
            normalize_cluster_url("https://example.invalid/clusters/prod")
        );
    }
}
