//! Docker context membership, shared by execution-key hashing and staged builds.
//! Docker's rules: https://docs.docker.com/build/concepts/context/#dockerignore-files
use nyl_core::orchestration::driver::{DriverError, DriverResult};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

struct Rule {
    pattern: Regex,
    include: bool,
}

fn pattern(value: &str) -> DriverResult<Regex> {
    let mut regex = String::from("^");
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    regex.push_str("(?:.*/)?");
                } else {
                    regex.push_str(".*");
                }
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '[' => {
                regex.push('[');
                if chars.peek() == Some(&'^') {
                    regex.push('^');
                    chars.next();
                }
                let mut closed = false;
                for next in chars.by_ref() {
                    regex.push(next);
                    if next == ']' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(DriverError::before_effects(format!(
                        "invalid Docker ignore pattern {value:?}"
                    )));
                }
            }
            '\\' => {
                let next = chars
                    .next()
                    .ok_or_else(|| DriverError::before_effects("Docker ignore pattern ends with an escape"))?;
                regex.push_str(&regex::escape(&next.to_string()));
            }
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push('$');
    Regex::new(&regex).map_err(|_| DriverError::before_effects(format!("invalid Docker ignore pattern {value:?}")))
}

fn rules(text: &str) -> DriverResult<Vec<Rule>> {
    let mut result = Vec::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        if line.starts_with('#') {
            continue;
        }
        let line = line.trim();
        let (include, line) = line.strip_prefix('!').map_or((false, line), |line| (true, line));
        if line.is_empty() {
            if include {
                return Err(DriverError::before_effects("empty Docker ignore exception"));
            }
            continue;
        }
        // filepath.Clean: remove dot segments and normalize parents before matching.
        let mut segments = Vec::new();
        for segment in line.trim_matches('/').split('/') {
            match segment {
                "" | "." => {}
                ".." if segments.last().is_some_and(|part| *part != "..") => {
                    segments.pop();
                }
                _ => segments.push(segment),
            }
        }
        let line = segments.join("/");
        if line.is_empty() || line == "." {
            continue;
        }
        result.push(Rule {
            pattern: pattern(&line)?,
            include,
        });
    }
    Ok(result)
}

fn included(path: &str, rules: &[Rule]) -> bool {
    let mut include = true;
    for rule in rules {
        let mut part = path;
        loop {
            if rule.pattern.is_match(part) {
                include = rule.include;
                break;
            }
            let Some((parent, _)) = part.rsplit_once('/') else {
                break;
            };
            part = parent;
        }
    }
    include
}

/// A context's admitted members, including empty directories and Unix modes.
/// `.git` is checkout administration, never an image build input.
pub(crate) struct ContextFiles {
    root: PathBuf,
    pub(crate) digests: BTreeMap<String, String>,
    members: Vec<PathBuf>,
}
impl ContextFiles {
    #[allow(clippy::too_many_lines)] // Read membership and ancestor metadata together before the staged build.
    pub(crate) fn read(root: &Path, dockerfile: Option<&str>) -> DriverResult<Self> {
        if !root.is_dir() {
            return Err(DriverError::before_effects(format!(
                "image context {} is not a directory",
                root.display()
            )));
        }
        let specific = dockerfile.map(|file| format!("{file}.dockerignore"));
        let ignore = specific
            .as_deref()
            .filter(|file| root.join(file).is_file())
            .unwrap_or(".dockerignore");
        let text = match std::fs::read_to_string(root.join(ignore)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(DriverError::before_effects(format!(
                    "cannot read Docker ignore file: {error}"
                )))
            }
        };
        let rules = rules(&text)?;
        let mut files = Self {
            root: root.to_owned(),
            digests: BTreeMap::new(),
            members: Vec::new(),
        };
        for entry in walkdir::WalkDir::new(root)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git")
        {
            let entry = entry.map_err(|error| DriverError::before_effects(error.to_string()))?;
            let path = entry.path().strip_prefix(root).expect("context member");
            let name = path
                .to_str()
                .ok_or_else(|| DriverError::before_effects("Docker context paths must be UTF-8"))?
                .replace('\\', "/");
            let forced = dockerfile == Some(name.as_str()) || name == ignore;
            if !forced && !included(&name, &rules) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| DriverError::before_effects(error.to_string()))?;
            if metadata.file_type().is_symlink() {
                return Err(DriverError::before_effects(format!(
                    "image context cannot traverse symlink {name}"
                )));
            }
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(DriverError::before_effects(format!(
                    "unsupported image context file {name}"
                )));
            }
            let mut bytes = if metadata.is_file() {
                std::fs::read(entry.path()).map_err(|error| DriverError::before_effects(error.to_string()))?
            } else {
                Vec::new()
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                bytes.extend_from_slice(&(metadata.permissions().mode() & 0o7777).to_le_bytes());
            }
            let name = if metadata.is_dir() { format!("{name}/") } else { name };
            files.digests.insert(name, nyl_core::digest::sha256_prefixed(&bytes));
            files.members.push(path.to_owned());
        }
        // Re-included children still expose their ancestor directories and modes.
        let mut parents = std::collections::BTreeSet::new();
        for member in &files.members {
            let mut parent = member.parent();
            while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
                parents.insert(path.to_owned());
                parent = path.parent();
            }
        }
        for parent in parents {
            let name = format!("{}/", parent.to_str().expect("UTF-8 member").replace('\\', "/"));
            if files.digests.contains_key(&name) {
                continue;
            }
            let metadata = std::fs::metadata(root.join(&parent))
                .map_err(|error| DriverError::before_effects(error.to_string()))?;
            let mut bytes = Vec::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                bytes.extend_from_slice(&(metadata.permissions().mode() & 0o7777).to_le_bytes());
            }
            files.digests.insert(name, nyl_core::digest::sha256_prefixed(&bytes));
            files.members.push(parent);
        }
        files.members.sort();
        if let Some(dockerfile) = dockerfile {
            if !root.join(dockerfile).is_file() {
                return Err(DriverError::before_effects(format!(
                    "Dockerfile {dockerfile:?} does not exist in {}",
                    root.display()
                )));
            }
        }
        Ok(files)
    }

    /// Copy precisely the hashed context so Git checkout metadata and ignored
    /// files cannot affect a build without affecting its execution key.
    pub(crate) fn stage(&self, destination: &Path) -> DriverResult<()> {
        let perform = || -> std::io::Result<()> {
            std::fs::create_dir_all(destination)?;
            for relative in &self.members {
                if relative.components().any(|part| !matches!(part, Component::Normal(_))) {
                    return Err(std::io::Error::other("unsafe context member"));
                }
                let source = self.root.join(relative);
                let target = destination.join(relative);
                if source.is_dir() {
                    std::fs::create_dir_all(&target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(source, target)?;
                }
            }
            // Apply directory modes after their children have been copied.
            for relative in self.members.iter().rev() {
                let source = self.root.join(relative);
                std::fs::set_permissions(destination.join(relative), std::fs::metadata(source)?.permissions())?;
            }
            Ok(())
        };
        perform().map_err(|error| DriverError::before_effects(format!("cannot stage image context: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_ignore_rules_match_docker_root_patterns_parents_and_exceptions() {
        let rules = rules("*.md\ncache\n**/tmp\n!cache/keep.txt\n").unwrap();
        for path in ["README.md", "cache/drop.txt", "a/tmp/child", "tmp/child"] {
            assert!(!included(path, &rules), "{path}");
        }
        for path in ["docs/README.md", "cache/keep.txt", "a/file"] {
            assert!(included(path, &rules), "{path}");
        }
    }
    #[test]
    fn test_dockerfile_ignore_precedence_and_staged_members_equal_key_inputs() {
        let root = tempfile::TempDir::new().unwrap();
        for (name, bytes) in [
            ("Dockerfile", "FROM scratch"),
            (".dockerignore", "keep\n"),
            ("Dockerfile.dockerignore", "drop\n"),
            ("keep", "yes"),
            ("drop", "no"),
            (".git", "checkout-specific metadata"),
        ] {
            std::fs::write(root.path().join(name), bytes).unwrap();
        }
        std::fs::create_dir(root.path().join("empty")).unwrap();
        let files = ContextFiles::read(root.path(), Some("Dockerfile")).unwrap();
        assert!(files.digests.contains_key("keep"));
        assert!(files.digests.contains_key("empty/"));
        assert!(!files.digests.contains_key("drop"));
        assert!(!files.digests.contains_key(".git"));
        let staged = tempfile::TempDir::new().unwrap();
        files.stage(staged.path()).unwrap();
        assert_eq!(
            files.digests,
            ContextFiles::read(staged.path(), Some("Dockerfile")).unwrap().digests
        );
        std::fs::write(root.path().join("drop"), "changed ignored file").unwrap();
        assert_eq!(
            files.digests,
            ContextFiles::read(root.path(), Some("Dockerfile")).unwrap().digests
        );
    }
}
