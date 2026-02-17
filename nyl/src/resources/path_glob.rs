use glob::Pattern;

use crate::{NylError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternSegment {
    DoubleStar,
    Segment(String),
}

/// Validate a dotted field-path glob pattern.
pub fn validate_path_glob_pattern(pattern: &str) -> Result<()> {
    let _ = parse_pattern(pattern)?;
    Ok(())
}

/// Match a concrete dotted path against a dotted glob pattern.
///
/// Rules:
/// - `*` matches any characters within a single segment.
/// - `**` matches zero or more full segments.
pub fn path_matches_glob(path: &str, pattern: &str) -> Result<bool> {
    let path_segments = parse_concrete_path(path)?;
    let pattern_segments = parse_pattern(pattern)?;
    Ok(matches_segments(&path_segments, &pattern_segments, 0, 0))
}

/// Join path segments into normalized dotted notation.
///
/// Segments containing non-identifier chars are quoted.
pub fn join_segments(segments: &[String]) -> String {
    segments
        .iter()
        .map(|segment| {
            if is_simple_identifier(segment) {
                segment.clone()
            } else {
                let escaped = segment.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{}\"", escaped)
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn is_simple_identifier(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_concrete_path(path: &str) -> Result<Vec<String>> {
    let segments = split_segments(path)?;
    if segments.is_empty() {
        return Err(NylError::Config("Field path must not be empty".to_string()));
    }

    if segments.iter().any(|segment| segment == "**") {
        return Err(NylError::Config(format!(
            "Concrete field path must not contain '**': {}",
            path
        )));
    }

    Ok(segments)
}

fn parse_pattern(pattern: &str) -> Result<Vec<PatternSegment>> {
    let segments = split_segments(pattern)?;
    if segments.is_empty() {
        return Err(NylError::Config("Path pattern must not be empty".to_string()));
    }

    let mut parsed = Vec::with_capacity(segments.len());
    for segment in segments {
        if segment == "**" {
            parsed.push(PatternSegment::DoubleStar);
            continue;
        }
        if segment.contains("**") {
            return Err(NylError::Config(format!(
                "Invalid path pattern segment '{}': '**' must be a standalone segment",
                segment
            )));
        }
        Pattern::new(&segment)
            .map_err(|e| NylError::Config(format!("Invalid path pattern segment '{}': {}", segment, e)))?;
        parsed.push(PatternSegment::Segment(segment));
    }

    Ok(parsed)
}

fn split_segments(input: &str) -> Result<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escape = false;

    for ch in input.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' if in_quotes => {
                escape = true;
            }
            '"' => {
                in_quotes = !in_quotes;
            }
            '.' if !in_quotes => {
                if current.is_empty() {
                    return Err(NylError::Config(format!("Invalid path '{}': empty segment", input)));
                }
                segments.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }

    if in_quotes {
        return Err(NylError::Config(format!("Invalid path '{}': unmatched quote", input)));
    }
    if escape {
        return Err(NylError::Config(format!("Invalid path '{}': dangling escape", input)));
    }
    if current.is_empty() {
        return Err(NylError::Config(format!(
            "Invalid path '{}': empty trailing segment",
            input
        )));
    }

    segments.push(current);
    Ok(segments)
}

fn matches_segments(path: &[String], pattern: &[PatternSegment], pi: usize, si: usize) -> bool {
    if si == pattern.len() {
        return pi == path.len();
    }

    match &pattern[si] {
        PatternSegment::DoubleStar => {
            if matches_segments(path, pattern, pi, si + 1) {
                return true;
            }
            let mut idx = pi;
            while idx < path.len() {
                if matches_segments(path, pattern, idx + 1, si) {
                    return true;
                }
                idx += 1;
            }
            false
        }
        PatternSegment::Segment(pattern_segment) => {
            if pi >= path.len() {
                return false;
            }
            let Ok(glob) = Pattern::new(pattern_segment) else {
                return false;
            };
            if glob.matches(&path[pi]) {
                matches_segments(path, pattern, pi + 1, si + 1)
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_matches_glob_single_segment_star() {
        assert!(path_matches_glob("spec.syncPolicy.automated", "spec.*.automated").unwrap());
        assert!(!path_matches_glob("spec.syncPolicy.automated.prune", "spec.*.automated").unwrap());
    }

    #[test]
    fn test_path_matches_glob_double_star() {
        assert!(path_matches_glob("spec.syncPolicy", "spec.syncPolicy.**").unwrap());
        assert!(path_matches_glob("spec.syncPolicy.automated.prune", "spec.syncPolicy.**").unwrap());
    }

    #[test]
    fn test_path_matches_glob_quoted_segment() {
        assert!(path_matches_glob(
            "metadata.annotations.\"pref.argocd.argoproj.io/foo\"",
            "metadata.annotations.\"pref.argocd.argoproj.io/*\""
        )
        .unwrap());
    }

    #[test]
    fn test_join_segments_quotes_special_segments() {
        let joined = join_segments(&[
            "metadata".to_string(),
            "annotations".to_string(),
            "pref.argocd.argoproj.io/foo".to_string(),
        ]);
        assert_eq!(joined, "metadata.annotations.\"pref.argocd.argoproj.io/foo\"");
    }
}
