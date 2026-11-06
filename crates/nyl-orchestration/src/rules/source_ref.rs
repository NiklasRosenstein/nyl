//! The `--source-ref` of an instance: where a host serves its recorded source
//! commit from when it will not serve it by its id.
//!
//! Contract: [Instances](../../../../design/orchestration-core.md#instances),
//! `--source-ref`. The ref is a hint recorded in `state.yaml` next to
//! `sourceCommit`: a full ref name under `refs/` of the source repository's
//! `origin`, such as GitHub's `refs/pull/<n>/head`. It is validated by
//! its syntax alone (Git's ref-name rules); nothing contacts the remote to
//! check it, and it is never resolved to a commit: the recorded commit is what
//! renders and tears down, whatever the ref points at later.

/// Why `reference` is not a usable ref name, or `Ok` when its syntax is fine.
///
/// It must start with `refs/` and have something after it, and follow
/// `git check-ref-format`: no empty, dot-leading, or `.lock`-ending component,
/// no `..`, `@{`, control character, space, or any of `~ ^ : ? * [ \`, and no
/// trailing `/` or `.`.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // Git refuses exactly `.lock`, not `.LOCK`.
pub fn validate(reference: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("{reference:?} is not a usable ref name: {why}"));
    let Some(rest) = reference.strip_prefix("refs/") else {
        return invalid("it must be a full ref name that starts with refs/");
    };
    if rest.is_empty() {
        return invalid("nothing follows refs/");
    }
    if reference.contains("..") || reference.contains("@{") || reference.ends_with('.') {
        return invalid("it contains `..`, `@{`, or ends with `.`");
    }
    if let Some(bad) = reference
        .chars()
        .find(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(*c))
    {
        return invalid(&format!("it contains {bad:?}"));
    }
    for component in rest.split('/') {
        if component.is_empty() {
            return invalid("it has an empty component (a leading, trailing, or doubled `/`)");
        }
        if component.starts_with('.') || component.ends_with(".lock") {
            return invalid("a component starts with `.` or ends with `.lock`");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a hint that is no ref being recorded, which a fetch would only
    /// fail on later (or, worse, treat as an option): the host names that
    /// carry a pull request are accepted and everything Git itself refuses,
    /// plus anything outside `refs/`, is rejected by syntax alone.
    #[test]
    fn test_only_full_ref_names_are_accepted() {
        for valid in [
            "refs/pull/123/head",
            "refs/merge-requests/7/head",
            "refs/heads/feature/x",
            "refs/changes/45/12345/1",
        ] {
            assert_eq!(validate(valid), Ok(()), "{valid}");
        }
        for invalid in [
            "",
            "pull/123/head",
            "heads/main",
            "main",
            "refs/",
            "refs//head",
            "refs/pull/1/head/",
            "refs/pull/../head",
            "refs/pull/.hidden",
            "refs/pull/1.lock",
            "refs/pull/1 head",
            "refs/pull/1~1",
            "refs/pull/1:head",
            "refs/pull/*",
            "refs/pull/1@{u}",
            "refs/pull/1\nhead",
            "--upload-pack=x",
            "refs/pull/1.",
        ] {
            assert!(validate(invalid).is_err(), "{invalid:?}");
        }
    }
}
