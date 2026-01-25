//! Integration tests for ArgoCD secret discovery
//!
//! Note: These tests require a mock Kubernetes API server or actual Kubernetes cluster.
//! For now, we test the URL matching logic which is the core functionality.

use nyl::git::argocd::{matches_repo_creds_pattern, matches_repository_url};

#[test]
fn test_matches_repository_url_exact() {
    // Exact match
    assert!(matches_repository_url(
        "https://github.com/org/repo",
        "https://github.com/org/repo"
    ));

    // Exact match with .git suffix
    assert!(matches_repository_url(
        "https://github.com/org/repo.git",
        "https://github.com/org/repo"
    ));

    // Exact match - both have .git
    assert!(matches_repository_url(
        "https://github.com/org/repo.git",
        "https://github.com/org/repo.git"
    ));
}

#[test]
fn test_matches_repository_url_ssh_shorthand() {
    // SSH shorthand to full SSH URL
    assert!(matches_repository_url(
        "git@github.com:org/repo",
        "ssh://git@github.com/org/repo"
    ));

    // SSH shorthand both sides
    assert!(matches_repository_url(
        "git@github.com:org/repo",
        "git@github.com:org/repo"
    ));

    // SSH shorthand with .git
    assert!(matches_repository_url(
        "git@github.com:org/repo.git",
        "git@github.com:org/repo"
    ));
}

#[test]
fn test_matches_repository_url_hostname_fallback() {
    // Different repos, same hostname - should match via fallback
    assert!(matches_repository_url(
        "https://github.com/org/repo1",
        "https://github.com/org/repo2"
    ));

    // Different repos, different hostname - should NOT match
    assert!(!matches_repository_url(
        "https://github.com/org/repo",
        "https://gitlab.com/org/repo"
    ));
}

#[test]
fn test_matches_repository_url_mixed_protocols() {
    // HTTPS vs SSH - same host
    assert!(matches_repository_url(
        "https://github.com/org/repo",
        "git@github.com:org/repo"
    ));

    // SSH shorthand vs HTTPS
    assert!(matches_repository_url(
        "git@github.com:org/repo",
        "https://github.com/org/repo"
    ));
}

#[test]
fn test_matches_repository_url_case_insensitive() {
    // Case insensitive hostname
    assert!(matches_repository_url(
        "https://GitHub.com/org/repo",
        "https://github.com/org/repo"
    ));

    // Case insensitive full URL
    assert!(matches_repository_url(
        "https://GitHub.com/Org/Repo",
        "https://github.com/org/repo"
    ));
}

#[test]
fn test_matches_repository_url_trailing_slash() {
    // With and without trailing slash
    assert!(matches_repository_url(
        "https://github.com/org/repo/",
        "https://github.com/org/repo"
    ));

    // Both with trailing slash
    assert!(matches_repository_url(
        "https://github.com/org/repo/",
        "https://github.com/org/repo/"
    ));
}

#[test]
fn test_matches_repository_url_different_providers() {
    // GitHub vs GitLab - should NOT match
    assert!(!matches_repository_url(
        "https://github.com/org/repo",
        "https://gitlab.com/org/repo"
    ));

    // GitHub vs Gitea - should NOT match
    assert!(!matches_repository_url(
        "https://github.com/org/repo",
        "https://gitea.example.com/org/repo"
    ));
}

#[test]
fn test_matches_repository_url_subpaths() {
    // Different subpaths, same host - should match via hostname fallback
    assert!(matches_repository_url(
        "https://github.com/org1/repo1",
        "https://github.com/org2/repo2"
    ));

    // Same org, different repo
    assert!(matches_repository_url(
        "https://github.com/org/repo1",
        "https://github.com/org/repo2"
    ));
}

// Note: Full ArgoCD credential discovery tests would require:
// 1. Mock Kubernetes API server
// 2. Test secret creation and querying
// 3. Test credential extraction from secrets
// 4. Test base64 decoding
// 5. Test error handling for malformed secrets
//
// These would be better suited for end-to-end tests with a real or mock K8s cluster.
// For now, the URL matching logic is the critical path and is thoroughly tested above.

// ============================================================================
// Pattern Matching Tests for repo-creds
// ============================================================================

#[test]
fn test_matches_repo_creds_pattern_simple_wildcard() {
    // Simple wildcard at end - org level
    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*",
        "https://github.com/myorg/repo1"
    ));

    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*",
        "https://github.com/myorg/charts"
    ));

    // Should NOT match different org
    assert!(!matches_repo_creds_pattern(
        "https://github.com/myorg/*",
        "https://github.com/otherorg/repo"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_broader_wildcard() {
    // Wildcard at higher level - all of GitHub
    assert!(matches_repo_creds_pattern(
        "https://github.com/*",
        "https://github.com/anyorg/anyrepo"
    ));

    assert!(matches_repo_creds_pattern(
        "https://github.com/*",
        "https://github.com/myorg/charts"
    ));

    // Should NOT match different host
    assert!(!matches_repo_creds_pattern(
        "https://github.com/*",
        "https://gitlab.com/org/repo"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_ssh_shorthand() {
    // SSH shorthand with pattern
    assert!(matches_repo_creds_pattern(
        "git@github.com:myorg/*",
        "git@github.com:myorg/charts"
    ));

    // SSH shorthand pattern matching full SSH URL
    assert!(matches_repo_creds_pattern(
        "git@github.com:myorg/*",
        "ssh://git@github.com/myorg/charts"
    ));

    // Should NOT match different org
    assert!(!matches_repo_creds_pattern(
        "git@github.com:myorg/*",
        "git@github.com:otherorg/repo"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_with_git_suffix() {
    // Pattern with .git suffix
    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*.git",
        "https://github.com/myorg/repo.git"
    ));

    // Pattern without .git matching URL with .git
    // Both get normalized to remove .git
    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*",
        "https://github.com/myorg/repo.git"
    ));

    // URL without .git matching pattern with .git
    // Both get normalized
    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*.git",
        "https://github.com/myorg/repo"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_case_insensitive() {
    // Case insensitive matching
    assert!(matches_repo_creds_pattern(
        "https://GitHub.com/MyOrg/*",
        "https://github.com/myorg/repo"
    ));

    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*",
        "https://GitHub.com/MyOrg/Repo"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_multiple_levels() {
    // Pattern with nested paths
    assert!(matches_repo_creds_pattern(
        "https://gitlab.com/*/*",
        "https://gitlab.com/group/project"
    ));

    assert!(matches_repo_creds_pattern(
        "https://gitlab.com/mygroup/*",
        "https://gitlab.com/mygroup/subgroup/project"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_exact_vs_pattern() {
    // Exact URL should NOT match when pattern doesn't
    assert!(!matches_repo_creds_pattern(
        "https://github.com/myorg/specific-repo",
        "https://github.com/myorg/other-repo"
    ));

    // Pattern without wildcard is exact match
    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/repo",
        "https://github.com/myorg/repo"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_trailing_slash() {
    // With and without trailing slash
    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*/",
        "https://github.com/myorg/repo"
    ));

    assert!(matches_repo_creds_pattern(
        "https://github.com/myorg/*",
        "https://github.com/myorg/repo/"
    ));
}

#[test]
fn test_matches_repo_creds_pattern_special_chars() {
    // Pattern with special regex characters (should be escaped)
    assert!(matches_repo_creds_pattern(
        "https://git.example.com/my-org/*",
        "https://git.example.com/my-org/my-repo"
    ));

    // Dots should be treated as literal, not regex wildcard
    assert!(!matches_repo_creds_pattern(
        "https://git.example.com/*",
        "https://gitXexampleXcom/org/repo"
    ));
}

// ============================================================================
// Precedence Tests (Unit tests for the logic)
// ============================================================================

#[test]
fn test_precedence_exact_beats_pattern() {
    // Test that exact match is preferred over pattern match
    // This is a logical test - actual precedence is tested in integration tests
    let exact_url = "https://github.com/myorg/charts";
    let pattern = "https://github.com/myorg/*";

    // Exact match
    assert!(matches_repository_url(exact_url, exact_url));

    // Pattern match
    assert!(matches_repo_creds_pattern(pattern, exact_url));

    // Both match, but exact should win (tested in find_credential_for_url)
}

#[test]
fn test_precedence_pattern_beats_hostname() {
    // Test that pattern match is preferred over hostname fallback
    let url = "https://github.com/myorg/repo1";
    let pattern = "https://github.com/myorg/*";
    let different_repo = "https://github.com/otherorg/repo2";

    // Pattern match
    assert!(matches_repo_creds_pattern(pattern, url));

    // Hostname fallback would also match
    assert!(matches_repository_url(different_repo, url));

    // Pattern should win over hostname fallback (tested in find_credential_for_url)
}

#[test]
fn test_no_match_different_providers() {
    // Different providers should not match with patterns
    assert!(!matches_repo_creds_pattern(
        "https://github.com/*",
        "https://gitlab.com/org/repo"
    ));

    assert!(!matches_repo_creds_pattern(
        "git@github.com:*/*",
        "git@gitlab.com:org/repo"
    ));
}
