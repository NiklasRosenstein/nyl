/// Utility module tests
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_find_config_file_in_current_dir() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("config.yaml");
    fs::write(&config_path, "test").unwrap();

    let result = nyl::util::fs::find_config_file(&["config.yaml"], Some(temp.path()), false).unwrap();

    assert!(result.is_some());
    assert_eq!(result.unwrap(), config_path);
}

#[test]
fn test_find_config_file_in_parent_dir() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("config.yaml");
    fs::write(&config_path, "test").unwrap();

    let subdir = temp.path().join("subdir");
    fs::create_dir(&subdir).unwrap();

    let result = nyl::util::fs::find_config_file(&["config.yaml"], Some(&subdir), false).unwrap();

    assert!(result.is_some());
    assert_eq!(result.unwrap(), config_path);
}

#[test]
fn test_find_config_file_priority() {
    let temp = TempDir::new().unwrap();
    let yaml_path = temp.path().join("config.yaml");
    let json_path = temp.path().join("config.json");
    fs::write(&yaml_path, "yaml").unwrap();
    fs::write(&json_path, "json").unwrap();

    let result = nyl::util::fs::find_config_file(&["config.yaml", "config.json"], Some(temp.path()), false).unwrap();

    assert!(result.is_some());
    // YAML should be found first due to priority
    assert_eq!(result.unwrap(), yaml_path);
}

#[test]
fn test_find_config_file_not_found_optional() {
    let temp = TempDir::new().unwrap();

    let result = nyl::util::fs::find_config_file(&["missing.yaml"], Some(temp.path()), false).unwrap();

    assert!(result.is_none());
}

#[test]
fn test_find_config_file_not_found_required() {
    let temp = TempDir::new().unwrap();

    let result = nyl::util::fs::find_config_file(&["missing.yaml"], Some(temp.path()), true);

    assert!(result.is_err());
}

#[test]
fn test_resolve_path_absolute() {
    let base = PathBuf::from("/base/dir");
    let absolute = PathBuf::from("/absolute/path");

    let result = nyl::util::fs::resolve_path(&absolute, &base);
    assert_eq!(result, absolute);
}

#[test]
fn test_resolve_path_relative() {
    let base = PathBuf::from("/base/dir");
    let relative = PathBuf::from("relative/path");

    let result = nyl::util::fs::resolve_path(&relative, &base);
    assert_eq!(result, PathBuf::from("/base/dir/relative/path"));
}

#[test]
fn test_resolve_paths_multiple() {
    let base = PathBuf::from("/base/dir");
    let paths = vec![PathBuf::from("rel1"), PathBuf::from("/abs1"), PathBuf::from("rel2")];

    let results = nyl::util::fs::resolve_paths(&paths, &base);

    assert_eq!(results.len(), 3);
    assert_eq!(results[0], PathBuf::from("/base/dir/rel1"));
    assert_eq!(results[1], PathBuf::from("/abs1"));
    assert_eq!(results[2], PathBuf::from("/base/dir/rel2"));
}

#[test]
fn test_resolve_paths_empty() {
    let base = PathBuf::from("/base/dir");
    let paths: Vec<PathBuf> = vec![];

    let results = nyl::util::fs::resolve_paths(&paths, &base);
    assert!(results.is_empty());
}

#[test]
fn test_find_config_multiple_levels() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("config.yaml");
    fs::write(&config_path, "test").unwrap();

    // Create nested directories
    let level1 = temp.path().join("level1");
    let level2 = level1.join("level2");
    let level3 = level2.join("level3");
    fs::create_dir_all(&level3).unwrap();

    // Should find config from 3 levels up
    let result = nyl::util::fs::find_config_file(&["config.yaml"], Some(&level3), false).unwrap();

    assert!(result.is_some());
    assert_eq!(result.unwrap(), config_path);
}
