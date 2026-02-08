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

/// Convert a path to a file:// URL that works on both Unix and Windows
fn path_to_file_url(path: &Path) -> String {
    #[cfg(windows)]
    {
        // On Windows, convert backslashes to forward slashes and use three slashes
        // e.g., C:\Users\foo -> file:///C:/Users/foo
        let path_str = path.display().to_string().replace('\\', "/");
        format!("file:///{}", path_str)
    }
    #[cfg(not(windows))]
    {
        // On Unix, use two slashes (file:// + absolute path starting with /)
        format!("file://{}", path.display())
    }
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
    fs::write(repo_dir.join("test.txt"), "Hello, World!").expect("Failed to create test file");

    // Create a subdirectory with a file
    fs::create_dir_all(repo_dir.join("subdir")).expect("Failed to create subdir");
    fs::write(repo_dir.join("subdir/chart.yaml"), "name: test-chart").expect("Failed to create chart.yaml");

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

    // Set up cache directory (use explicit path instead of env var to avoid race conditions)
    let cache_dir = TempDir::new().unwrap();

    // Clone and resolve main branch
    let mut manager = GitManager::with_cache_dir(cache_dir.path());
    let result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
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

    let mut manager = GitManager::with_cache_dir(cache_dir.path());
    let result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), Some("subdir"))
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

    let mut manager = GitManager::with_cache_dir(cache_dir.path());
    let result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("test-branch"), None)
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

    let mut manager = GitManager::with_cache_dir(cache_dir.path());
    let result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("v1.0.0"), None)
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

    let mut manager = GitManager::with_cache_dir(cache_dir.path());

    // Resolve main branch
    let main_result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
        .unwrap();

    // Resolve test-branch
    let branch_result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("test-branch"), None)
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
        let mut manager = GitManager::with_cache_dir(&cache_path);
        let _result = manager
            .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
            .unwrap();
    }

    // Second resolution (should reuse cache)
    {
        let mut manager = GitManager::with_cache_dir(&cache_path);
        let result = manager
            .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
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

    let mut manager = GitManager::with_cache_dir(cache_dir.path());
    let _result = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
        .unwrap();

    // Verify cache structure
    let git_cache = cache_dir.path().join("git");
    assert!(git_cache.exists());

    let bare_dir = git_cache.join("bare");
    assert!(bare_dir.exists());

    let worktrees_dir = git_cache.join("worktrees");
    assert!(worktrees_dir.exists());

    // Should have one bare repo
    let bare_repos: Vec<_> = fs::read_dir(&bare_dir).unwrap().filter_map(|e| e.ok()).collect();
    assert_eq!(bare_repos.len(), 1);

    // Should have one worktree
    let worktrees: Vec<_> = fs::read_dir(&worktrees_dir).unwrap().filter_map(|e| e.ok()).collect();
    assert_eq!(worktrees.len(), 1);
}

/// Helper to create a test Git repository with a Helm chart
fn create_test_helm_chart_repo(repo_dir: &Path) {
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

    // Disable signing
    Command::new("git")
        .args(["config", "commit.gpgsign", "false"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to disable commit signing");

    // Create a Helm chart at the root
    fs::write(
        repo_dir.join("Chart.yaml"),
        "apiVersion: v2\nname: root-chart\nversion: 1.0.0\n",
    )
    .expect("Failed to create Chart.yaml");
    fs::write(repo_dir.join("values.yaml"), "key: value\n").expect("Failed to create values.yaml");

    // Create a Helm chart in a subdirectory
    fs::create_dir_all(repo_dir.join("charts/subchart")).expect("Failed to create charts/subchart");
    fs::write(
        repo_dir.join("charts/subchart/Chart.yaml"),
        "apiVersion: v2\nname: subchart\nversion: 2.0.0\n",
    )
    .expect("Failed to create subchart Chart.yaml");

    // Add and commit
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to git add");

    Command::new("git")
        .args(["commit", "-m", "Add Helm charts"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to git commit");

    // Create a tag
    Command::new("git")
        .args(["tag", "v1.0.0"])
        .current_dir(repo_dir)
        .output()
        .expect("Failed to create tag");
}

#[test]
fn test_git_chart_with_https_protocol_prefix() {
    setup_test_env();

    use nyl::helm::HelmChartResolver;
    use nyl::resources::ChartRef;

    let temp_repo = TempDir::new().unwrap();
    create_test_helm_chart_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();

    let resolver = HelmChartResolver::with_cache_dir(
        vec![],
        temp_repo.path().to_path_buf(),
        Some(cache_dir.path().to_path_buf()),
    );

    let chart_ref = ChartRef {
        repository: Some(format!("git+{}", path_to_file_url(temp_repo.path()))),
        version: Some("main".to_string()),
        name: None, // Chart at root
    };

    let resolved = resolver.resolve_chart(&chart_ref).unwrap();
    assert!(resolved.path.exists());
    assert!(resolved.path.join("Chart.yaml").exists());

    let content = fs::read_to_string(resolved.path.join("Chart.yaml")).unwrap();
    assert!(content.contains("name: root-chart"));
}

#[test]
fn test_git_chart_with_subpath() {
    setup_test_env();

    use nyl::helm::HelmChartResolver;
    use nyl::resources::ChartRef;

    let temp_repo = TempDir::new().unwrap();
    create_test_helm_chart_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();

    let resolver = HelmChartResolver::with_cache_dir(
        vec![],
        temp_repo.path().to_path_buf(),
        Some(cache_dir.path().to_path_buf()),
    );

    let chart_ref = ChartRef {
        repository: Some(format!("git+{}", path_to_file_url(temp_repo.path()))),
        version: Some("main".to_string()),
        name: Some("charts/subchart".to_string()), // Subpath in repo
    };

    let resolved = resolver.resolve_chart(&chart_ref).unwrap();
    assert!(resolved.path.exists());
    assert!(resolved.path.join("Chart.yaml").exists());

    let content = fs::read_to_string(resolved.path.join("Chart.yaml")).unwrap();
    assert!(content.contains("name: subchart"));
}

#[test]
fn test_git_chart_with_tag_version() {
    setup_test_env();

    use nyl::helm::HelmChartResolver;
    use nyl::resources::ChartRef;

    let temp_repo = TempDir::new().unwrap();
    create_test_helm_chart_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();

    let resolver = HelmChartResolver::with_cache_dir(
        vec![],
        temp_repo.path().to_path_buf(),
        Some(cache_dir.path().to_path_buf()),
    );

    let chart_ref = ChartRef {
        repository: Some(format!("git+{}", path_to_file_url(temp_repo.path()))),
        version: Some("v1.0.0".to_string()),
        name: None,
    };

    let resolved = resolver.resolve_chart(&chart_ref).unwrap();
    assert!(resolved.path.exists());
    assert!(resolved.path.join("Chart.yaml").exists());
}

#[test]
fn test_git_manager_fetches_latest_version() {
    setup_test_env();

    // Create a test repository
    let temp_repo = TempDir::new().unwrap();
    create_test_git_repo(temp_repo.path());

    let cache_dir = TempDir::new().unwrap();

    // First resolution - this will cache the repository
    let mut manager = GitManager::with_cache_dir(cache_dir.path());
    let result1 = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
        .unwrap();

    // Verify initial content
    let content1 = fs::read_to_string(result1.join("test.txt")).unwrap();
    assert_eq!(content1, "Hello, World!");

    // Now update the repository with a new commit on main branch
    fs::write(temp_repo.path().join("test.txt"), "Updated content!").expect("Failed to update file");
    let add_output = Command::new("git")
        .args(["add", "."])
        .current_dir(temp_repo.path())
        .output()
        .expect("Failed to launch git add");
    assert!(
        add_output.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let commit_output = Command::new("git")
        .args(["commit", "-m", "Update content"])
        .current_dir(temp_repo.path())
        .output()
        .expect("Failed to launch git commit");
    assert!(
        commit_output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Second resolution - should fetch and get the latest version
    // Reuse the same manager to simulate continuous usage
    let result2 = manager
        .resolve_ref(&path_to_file_url(temp_repo.path()), Some("main"), None)
        .unwrap();

    // Verify that we got the updated content
    let content2 = fs::read_to_string(result2.join("test.txt")).unwrap();
    assert_eq!(
        content2, "Updated content!",
        "Should fetch and checkout the latest version"
    );
}
