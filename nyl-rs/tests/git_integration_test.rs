/// Integration tests for Git repository management
///
/// These tests verify the end-to-end flow of Git operations including:
/// - Cloning bare repositories
/// - Creating worktrees
/// - Resolving refs
/// - HelmChart Git integration
/// - ApplicationGenerator Git integration

use nyl::git::GitManager;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Set up test environment to disable SSH and use only local operations
fn setup_test_env() {
    // Disable credential helpers and SSH prompts
    env::set_var("GIT_TERMINAL_PROMPT", "0");
    env::set_var("GIT_SSH_COMMAND", "echo 'SSH disabled in tests' && exit 1");
}

/// Helper to create a test Git repository with commits
fn create_test_git_repo(repo_dir: &Path) {
    // Initialize repository with explicit branch name
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to init git repo");

    // Configure user
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to set git user");

    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to set git email");

    // Disable credential helpers and SSH for this repo
    Command::new("git")
        .args(["config", "credential.helper", ""])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to disable credential helper");

    // Disable commit signing (prevents SSH agent prompts)
    Command::new("git")
        .args(["config", "commit.gpgsign", "false"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to disable commit signing");

    Command::new("git")
        .args(["config", "tag.gpgsign", "false"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to disable tag signing");

    // Create a test file
    fs::write(repo_dir.join("test.txt"), "Hello, World!")
        .expect("Failed to create test file");

    // Create a subdirectory with a file
    fs::create_dir_all(repo_dir.join("subdir")).expect("Failed to create subdir");
    fs::write(repo_dir.join("subdir/chart.yaml"), "name: test-chart")
        .expect("Failed to create chart.yaml");

    // Add and commit
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to git add");

    Command::new("git")
        .args(["commit", "-m", "Initial commit"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to git commit");

    // Create a branch
    Command::new("git")
        .args(["branch", "test-branch"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to create branch");

    // Create a tag
    Command::new("git")
        .args(["tag", "v1.0.0"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to create tag");
}

#[test]
fn test_git_manager_resolve_ref_main_branch() {
    setup_test_env();

    // Create a test repository
    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    // Set up cache directory
    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();
    std::env::set_var("NYL_CACHE_DIR", &cache_path);

    // Clone and resolve main branch
    let mut manager = GitManager::new().unwrap();
    let result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("main"),
            None,
        )
        .unwrap();

    // Verify the worktree exists
    assert!(result.exists());
    assert!(result.join("test.txt").exists());

    // Verify content
    let content = fs::read_to_string(result.join("test.txt")).unwrap();
    assert_eq!(content, "Hello, World!");
}

#[test]
fn test_git_manager_resolve_ref_with_subpath() {
    setup_test_env();

    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();
    std::env::set_var("NYL_CACHE_DIR", &cache_path);

    let mut manager = GitManager::new().unwrap();
    let result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("main"),
            Some("subdir"),
        )
        .unwrap();

    // Verify we're in the subdirectory
    assert!(result.exists());
    assert!(result.join("chart.yaml").exists());

    let content = fs::read_to_string(result.join("chart.yaml")).unwrap();
    assert_eq!(content, "name: test-chart");
}

#[test]
fn test_git_manager_resolve_ref_branch() {
    setup_test_env();

    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();
    std::env::set_var("NYL_CACHE_DIR", &cache_path);

    let mut manager = GitManager::new().unwrap();
    let result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("test-branch"),
            None,
        )
        .unwrap();

    assert!(result.exists());
    assert!(result.join("test.txt").exists());
}

#[test]
fn test_git_manager_resolve_ref_tag() {
    setup_test_env();

    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();
    std::env::set_var("NYL_CACHE_DIR", &cache_path);

    let mut manager = GitManager::new().unwrap();
    let result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("v1.0.0"),
            None,
        )
        .unwrap();

    assert!(result.exists());
    assert!(result.join("test.txt").exists());
}

#[test]
fn test_git_manager_multiple_refs_same_repo() {
    setup_test_env();

    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();
    std::env::set_var("NYL_CACHE_DIR", &cache_path);

    let mut manager = GitManager::new().unwrap();

    // Resolve main branch
    let main_result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("main"),
            None,
        )
        .unwrap();

    // Resolve test-branch
    let branch_result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("test-branch"),
            None,
        )
        .unwrap();

    // Both should exist and be different paths (different worktrees)
    assert!(main_result.exists());
    assert!(branch_result.exists());
    assert_ne!(main_result, branch_result);

    // Both should have the test file
    assert!(main_result.join("test.txt").exists());
    assert!(branch_result.join("test.txt").exists());
}

#[test]
fn test_git_manager_cache_reuse() {
    setup_test_env();

    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();

    // First resolution
    {
        std::env::set_var("NYL_CACHE_DIR", &cache_path);
        let mut manager = GitManager::new().unwrap();
        let _result = manager
            .resolve_ref(
                &format!("file://{}", temp_repo.path().display()),
                Some("main"),
                None,
            )
            .unwrap();
    }

    // Second resolution (should reuse cache)
    // Re-set the env var to ensure it hasn't been changed by parallel tests
    {
        std::env::set_var("NYL_CACHE_DIR", &cache_path);
        let mut manager = GitManager::new().unwrap();
        let result = manager
            .resolve_ref(
                &format!("file://{}", temp_repo.path().display()),
                Some("main"),
                None,
            )
            .unwrap();

        assert!(result.exists());
        assert!(result.join("test.txt").exists());
    }
}

#[test]
fn test_cache_directory_structure() {
    setup_test_env();

    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().to_path_buf();
    std::env::set_var("NYL_CACHE_DIR", &cache_path);

    let mut manager = GitManager::new().unwrap();
    let _result = manager
        .resolve_ref(
            &format!("file://{}", temp_repo.path().display()),
            Some("main"),
            None,
        )
        .unwrap();

    // Verify cache structure
    let git_cache = cache_dir.path().join("git");
    assert!(git_cache.exists());

    let bare_dir = git_cache.join("bare");
    assert!(bare_dir.exists());

    let worktrees_dir = git_cache.join("worktrees");
    assert!(worktrees_dir.exists());

    // Should have one bare repo
    let bare_repos: Vec<_> = fs::read_dir(&bare_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(bare_repos.len(), 1);

    // Should have one worktree
    let worktrees: Vec<_> = fs::read_dir(&worktrees_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(worktrees.len(), 1);
}
