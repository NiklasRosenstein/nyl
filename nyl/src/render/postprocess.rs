//! Release post-processing and deduplication.

use crate::{config::StripEmptyMetadataLabelsMode, kubernetes::ResourceKey, resources::Release, Result};

/// Deduplicate Kubernetes identities with last occurrence winning.
///
/// The returned map contains each duplicated identity and its total occurrence
/// count.
pub(crate) fn deduplicate_manifests(
    manifests: Vec<serde_json::Value>,
) -> Result<(Vec<serde_json::Value>, std::collections::HashMap<ResourceKey, usize>)> {
    use std::collections::HashMap;

    let mut seen: HashMap<ResourceKey, usize> = HashMap::new();
    let mut deduplicated = Vec::new();
    let mut duplicate_counts: HashMap<ResourceKey, usize> = HashMap::new();

    for manifest in manifests {
        let key = ResourceKey::from_json_value(&manifest)?;

        if let Some(prev_index) = seen.get(&key) {
            // Duplicate found - replace the previous one with this one (last occurrence wins)
            tracing::warn!("Duplicate resource: {} (keeping last occurrence)", key);
            deduplicated[*prev_index] = manifest;
            // Increment count for this duplicate
            *duplicate_counts.entry(key).or_insert(1) += 1;
        } else {
            // First occurrence - initialize count to 1
            duplicate_counts.insert(key.clone(), 1);
            seen.insert(key, deduplicated.len());
            deduplicated.push(manifest);
        }
    }

    // Filter to only resources with count > 1 (actual duplicates)
    let duplicates: HashMap<_, _> = duplicate_counts.into_iter().filter(|(_, count)| *count > 1).collect();

    Ok((deduplicated, duplicates))
}

pub(crate) fn best_effort_parse_yaml_documents(raw: &str) -> Vec<serde_json::Value> {
    let mut docs = Vec::new();
    for doc_str in split_yaml_documents(raw) {
        let trimmed = doc_str.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(parsed) = crate::yaml::parse_yaml_documents_k8s_compatible(trimmed) {
            docs.extend(parsed);
        }
    }
    docs
}

/// Split a YAML string into individual document strings by `---` separators.
pub(crate) fn split_yaml_documents(raw: &str) -> Vec<&str> {
    let mut docs = Vec::new();
    let mut start = 0;
    let mut offset = 0;

    for line in raw.split_inclusive('\n') {
        let line_content = line.trim_end_matches(['\r', '\n']);
        if line_content == "---" {
            if offset > start {
                docs.push(&raw[start..offset]);
            }
            start = offset + line.len();
        }
        offset += line.len();
    }

    // Handle last line without trailing newline
    if offset > start {
        let remainder = &raw[start..offset];
        if !remainder.trim().is_empty() {
            docs.push(remainder);
        }
    }
    docs
}

pub(crate) fn normalize_emitted_manifests(manifests: &mut [serde_json::Value]) {
    for manifest in manifests {
        strip_empty_metadata_labels(manifest);
    }
}

pub(crate) fn resolve_strip_empty_metadata_labels_mode(
    project_mode: StripEmptyMetadataLabelsMode,
    release: Option<&Release>,
) -> StripEmptyMetadataLabelsMode {
    release
        .and_then(|release| release.spec.strip_empty_metadata_labels)
        .unwrap_or(project_mode)
}

pub(crate) fn prepare_manifests_for_output(
    manifests: &[serde_json::Value],
    strip_empty_metadata_labels: bool,
) -> Vec<serde_json::Value> {
    let mut emitted_manifests = manifests.to_vec();
    if strip_empty_metadata_labels {
        normalize_emitted_manifests(&mut emitted_manifests);
    }
    emitted_manifests
}

fn strip_empty_metadata_labels(manifest: &mut serde_json::Value) {
    let Some(metadata) = manifest.get_mut("metadata").and_then(|value| value.as_object_mut()) else {
        return;
    };

    let should_remove = metadata
        .get("labels")
        .and_then(|value| value.as_object())
        .is_some_and(serde_json::Map::is_empty);

    if should_remove {
        metadata.remove("labels");
    }
}
