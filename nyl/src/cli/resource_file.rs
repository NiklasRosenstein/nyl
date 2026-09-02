use std::fs;
use std::io::Write as _;
use std::ops::Range;
use std::path::Path;

use crate::{NylError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentRange {
    content: Range<usize>,
    segment: Range<usize>,
}

pub(crate) fn append_document(contents: &str, document: &str) -> String {
    let mut output = contents.to_owned();
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if !output.is_empty() {
        output.push_str("---\n");
    }
    output.push_str(document);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

pub(crate) fn replace_document(
    contents: &str,
    document_index: usize,
    expected: &str,
    replacement: &str,
) -> Result<String> {
    let range = selected_range(contents, document_index, expected)?;
    let mut output = String::with_capacity(contents.len() - range.content.len() + replacement.len());
    output.push_str(&contents[..range.content.start]);
    output.push_str(replacement);
    output.push_str(&contents[range.content.end..]);
    Ok(output)
}

pub(crate) fn remove_document(contents: &str, document_index: usize, expected: &str) -> Result<String> {
    let ranges = document_ranges(contents);
    let range = selected_range_from(&ranges, contents, document_index, expected)?;
    let mut output = String::with_capacity(contents.len() - range.segment.len());
    output.push_str(&contents[..range.segment.start]);
    output.push_str(&contents[range.segment.end..]);
    if document_index == 1 {
        output = strip_leading_document_marker(output);
    }
    Ok(output)
}

pub(crate) fn document_count(contents: &str) -> usize {
    document_ranges(contents).len()
}

pub(crate) fn atomic_replace(path: &Path, expected: &str, replacement: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NylError::config(format!("{} has no parent directory", path.display())))?;
    if fs::read_to_string(path)? != expected {
        return Err(NylError::config(format!(
            "{} changed while it was being edited; refusing to overwrite it",
            path.display()
        )));
    }
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(NylError::config(format!("Refusing to edit symlink {}", path.display())));
    }
    let permissions = fs::metadata(path)?.permissions();
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(replacement.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map(|_| ()).map_err(|error| error.error.into())
}

fn selected_range(contents: &str, document_index: usize, expected: &str) -> Result<DocumentRange> {
    let ranges = document_ranges(contents);
    selected_range_from(&ranges, contents, document_index, expected)
}

fn selected_range_from(
    ranges: &[DocumentRange],
    contents: &str,
    document_index: usize,
    expected: &str,
) -> Result<DocumentRange> {
    let range = document_index
        .checked_sub(1)
        .and_then(|index| ranges.get(index))
        .ok_or_else(|| NylError::config(format!("YAML document {document_index} no longer exists")))?;
    if &contents[range.content.clone()] != expected {
        return Err(NylError::config(format!(
            "YAML document {document_index} changed while it was being edited"
        )));
    }
    Ok(range.clone())
}

fn document_ranges(contents: &str) -> Vec<DocumentRange> {
    let mut ranges = Vec::new();
    let mut content_start = 0;
    let mut segment_start = 0;
    let mut offset = 0;

    for line in contents.split_inclusive('\n') {
        if is_document_marker(line) {
            if offset > content_start {
                ranges.push(DocumentRange {
                    content: content_start..offset,
                    segment: segment_start..offset,
                });
            }
            segment_start = offset;
            content_start = offset + line.len();
        }
        offset += line.len();
    }
    if content_start < contents.len() {
        ranges.push(DocumentRange {
            content: content_start..contents.len(),
            segment: segment_start..contents.len(),
        });
    }
    ranges
}

fn is_document_marker(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    let line = line.trim_end_matches(['\r', '\n']).trim_end();
    ["---", "..."].into_iter().any(|marker| {
        line == marker
            || line
                .strip_prefix(marker)
                .is_some_and(|tail| tail.starts_with([' ', '#']))
    })
}

fn strip_leading_document_marker(contents: String) -> String {
    let marker_length = contents
        .split_inclusive('\n')
        .next()
        .filter(|line| is_document_marker(line))
        .map_or(0, str::len);
    contents[marker_length..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_a_separated_document() {
        assert_eq!(
            append_document("first: true\n", "second: true\n"),
            "first: true\n---\nsecond: true\n"
        );
    }

    #[test]
    fn replaces_only_the_selected_document() {
        let input = "first: true\n---\nsecond: true\n";
        assert_eq!(
            replace_document(input, 2, "second: true\n", "second: false\n").unwrap(),
            "first: true\n---\nsecond: false\n"
        );
    }

    #[test]
    fn removes_a_document_and_its_separator() {
        let input = "first: true\n---\nsecond: true\n---\nthird: true\n";
        assert_eq!(
            remove_document(input, 2, "second: true\n").unwrap(),
            "first: true\n---\nthird: true\n"
        );
    }

    #[test]
    fn removes_the_following_separator_with_the_first_document() {
        let input = "first: true\n---\nsecond: true\n";
        assert_eq!(remove_document(input, 1, "first: true\n").unwrap(), "second: true\n");
    }
}
