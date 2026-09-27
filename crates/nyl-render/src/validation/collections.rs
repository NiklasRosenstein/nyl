//! Complete inventories of pinned Kubernetes schema directories.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::resolve::{SchemaResolver, BUILTIN_REVISION};
use super::store;
use crate::{NylError, Result};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Collection {
    directory: String,
    files: BTreeSet<String>,
}

#[derive(Deserialize)]
struct GitTree {
    truncated: bool,
    tree: Vec<GitEntry>,
}

#[derive(Deserialize)]
struct GitEntry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

impl Collection {
    fn validate(&self, directory: &str) -> Result<()> {
        if self.directory != directory || self.files.is_empty() {
            return Err(NylError::validation(
                "Invalid builtin schema collection identity or empty inventory",
            ));
        }
        for name in &self.files {
            if std::path::Path::new(name)
                .extension()
                .is_none_or(|extension| extension != "json")
                || name
                    .bytes()
                    .any(|byte| !byte.is_ascii_alphanumeric() && !b"._-".contains(&byte))
            {
                return Err(NylError::validation("Unsafe builtin schema collection filename"));
            }
        }
        Ok(())
    }

    fn from_tree(directory: &str, tree: GitTree) -> Result<Self> {
        if tree.truncated {
            return Err(NylError::validation("Truncated builtin schema directory listing"));
        }
        let mut files = BTreeSet::new();
        for entry in tree.tree {
            if entry.kind == "tree" || entry.mode == "120000" {
                return Err(NylError::validation(
                    "Unexpected directory or symlink in builtin schema collection",
                ));
            }
            if std::path::Path::new(&entry.path)
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
                    return Err(NylError::validation("Invalid builtin schema collection entry"));
                }
                if !files.insert(entry.path) {
                    return Err(NylError::validation("Duplicate builtin schema collection entry"));
                }
            }
        }
        let collection = Self {
            directory: directory.into(),
            files,
        };
        collection.validate(directory)?;
        Ok(collection)
    }
}

impl SchemaResolver<'_> {
    /// Materialize or check a complete version directory and its dependencies.
    pub async fn vendor_version(&mut self, version: &str) -> Result<()> {
        let revision = self
            .settings
            .builtin_schema_revision
            .as_deref()
            .unwrap_or(BUILTIN_REVISION);
        let strict = if self.settings.strict { "-strict" } else { "" };
        let name = format!("v{version}-standalone{strict}");
        let directory = format!("https://raw.githubusercontent.com/yannh/kubernetes-json-schema/{revision}/{name}/");
        let collection = self.collection(&directory, &name, revision).await?;
        self.populate_collection(&collection).await?;
        let bytes = store::json_bytes(&collection)?;
        let hash = if self.check {
            store::digest(&bytes)
        } else {
            store::write_blob(&self.vendor, &bytes)?
        };
        self.observed_collections.insert(directory, hash);
        Ok(())
    }

    async fn collection(&self, directory: &str, name: &str, revision: &str) -> Result<Collection> {
        if !self.refresh {
            if let Some(hash) = store::read_builtins(&self.vendor)?.collections.get(directory) {
                let loaded = (|| {
                    let collection: Collection = serde_json::from_slice(&store::read_blob(&self.vendor, hash)?)?;
                    collection.validate(directory)?;
                    Ok(collection)
                })();
                match loaded {
                    Ok(collection) => return Ok(collection),
                    Err(error) if self.check => return Err(error),
                    Err(_) => {}
                }
            }
        }
        if self.check {
            return Err(NylError::validation(format!(
                "Missing vendored builtin schema collection {directory}; run nyl vendor"
            )));
        }
        let root = self.git_tree(revision).await?;
        if root.truncated {
            return Err(NylError::validation("Truncated builtin registry root listing"));
        }
        let entry = root
            .tree
            .iter()
            .find(|entry| entry.path == name && entry.kind == "tree" && entry.mode == "040000")
            .ok_or_else(|| NylError::validation(format!("Builtin registry has no schema directory {name}")))?;
        if entry.sha.len() != 40 || !entry.sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(NylError::validation("Invalid builtin registry subtree SHA"));
        }
        Collection::from_tree(directory, self.git_tree(&entry.sha).await?)
    }

    async fn git_tree(&self, sha: &str) -> Result<GitTree> {
        let url = format!("{}/{sha}", self.registry_api);
        let response = self
            .client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "nyl")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| NylError::validation(format!("Cannot list builtin schemas {url}: {error}")))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| NylError::validation(format!("Cannot read builtin listing {url}: {error}")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn populate_collection(&mut self, collection: &Collection) -> Result<()> {
        // Fetch roots before walking references so aliases and shared dependencies reuse observed blobs.
        let urls = collection
            .files
            .iter()
            .map(|name| format!("{}{name}", collection.directory))
            .collect::<Vec<_>>();
        for batch in urls.chunks(8) {
            let mut workers = tokio::task::JoinSet::new();
            for url in batch {
                if self.observed_builtins.contains_key(url) {
                    continue;
                }
                let project = self.project.to_path_buf();
                let vendor = self.vendor.clone();
                let settings = self.settings.clone();
                let (populate, check, refresh) = (self.populate, self.check, self.refresh);
                let url = url.clone();
                let client = self.client.clone();
                workers.spawn(async move {
                    let mut resolver =
                        SchemaResolver::new(&project, vendor, &settings, populate, check)?.with_refresh(refresh);
                    resolver.client = client;
                    resolver
                        .builtin(&url)
                        .await?
                        .ok_or_else(|| NylError::validation(format!("Missing builtin schema {url}")))?;
                    Ok::<_, NylError>(resolver.observed_builtins)
                });
            }
            while let Some(result) = workers.join_next().await {
                self.observed_builtins.extend(
                    result.map_err(|error| NylError::validation(format!("Schema download task failed: {error}")))??,
                );
            }
        }
        let stage = tempfile::TempDir::new()?;
        for url in urls {
            let document = self
                .builtin(&url)
                .await?
                .ok_or_else(|| NylError::validation(format!("Missing builtin schema {url}")))?;
            self.materialize(
                document,
                stage.path().join(format!("{}.json", store::digest(url.as_bytes()))),
                stage.path(),
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::{BuiltinSchemas, KubeconformSettings};
    use serde_json::json;
    use tempfile::TempDir;

    fn seed_collection(root: &std::path::Path, version: &str, strict: bool) -> Collection {
        let suffix = if strict { "-strict" } else { "" };
        let directory = format!("https://raw.githubusercontent.com/yannh/kubernetes-json-schema/{BUILTIN_REVISION}/v{version}-standalone{suffix}/");
        let collection = Collection {
            directory,
            files: BTreeSet::from([
                "configmap-v1.json".into(),
                "secret-v1.json".into(),
                "_definitions.json".into(),
            ]),
        };
        let mut index = store::read_builtins(root).unwrap();
        let hash = store::write_blob(root, &store::json_bytes(&collection).unwrap()).unwrap();
        index.collections.insert(collection.directory.clone(), hash);
        for file in &collection.files {
            let value = if file == "_definitions.json" {
                json!({"definitions":{"data":{"type":"object","additionalProperties":{"type":"string"}}}})
            } else {
                json!({"type":"object","title":file,"properties":{"data":{"$ref":"_definitions.json#/definitions/data"}}})
            };
            let hash = store::write_blob(root, &store::json_bytes(&value).unwrap()).unwrap();
            index.schemas.insert(format!("{}{file}", collection.directory), hash);
        }
        store::atomic_write(&root.join("schemas/builtins.json"), &store::json_bytes(&index).unwrap()).unwrap();
        collection
    }

    #[tokio::test]
    async fn test_all_versions_and_strictness_are_verified_offline_and_survive_pruning() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("vendor");
        for strict in [true, false] {
            for version in ["1.30.0", "1.31.4"] {
                seed_collection(&root, version, strict);
            }
        }
        for strict in [true, false] {
            let settings = KubeconformSettings {
                strict,
                builtin_schemas: BuiltinSchemas::VendorAll,
                ..Default::default()
            };
            let mut resolver = SchemaResolver::new(directory.path(), root.clone(), &settings, false, true).unwrap();
            for version in ["1.30.0", "1.31.4"] {
                resolver.vendor_version(version).await.unwrap();
                let url = resolver.builtin_url("v1/Secret", version).unwrap();
                assert!(resolver.builtin(&url).await.unwrap().is_some());
            }
            assert_eq!(resolver.observed_collections.len(), 2);
            assert_eq!(resolver.observed_builtins.len(), 6);
        }
        let unused = store::write_blob(&root, br#"{"unused":true}"#).unwrap();
        assert_eq!(store::check_and_prune(&root, true).unwrap(), 1);
        assert!(store::read_blob(&root, &unused).is_err());
        assert_eq!(store::check_and_prune(&root, false).unwrap(), 0);
    }

    #[tokio::test]
    async fn test_missing_unused_schema_or_collection_fails_offline_check() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("vendor");
        let collection = seed_collection(&root, "1.31.4", true);
        let settings = KubeconformSettings {
            builtin_schemas: BuiltinSchemas::VendorAll,
            ..Default::default()
        };
        let index = store::read_builtins(&root).unwrap();
        let hash = &index.schemas[&format!("{}secret-v1.json", collection.directory)];
        std::fs::remove_file(root.join(format!("schemas/blobs/{hash}.json"))).unwrap();
        let mut resolver = SchemaResolver::new(directory.path(), root.clone(), &settings, false, true).unwrap();
        assert!(resolver.vendor_version("1.31.4").await.is_err());
        seed_collection(&root, "1.31.4", true);
        let hash = &index.collections[&collection.directory];
        std::fs::write(root.join(format!("schemas/blobs/{hash}.json")), b"{}").unwrap();
        let mut resolver = SchemaResolver::new(directory.path(), root.clone(), &settings, false, true).unwrap();
        assert!(resolver.vendor_version("1.31.4").await.is_err());
        let mut resolver = SchemaResolver::new(directory.path(), root, &settings, false, true).unwrap();
        assert!(resolver
            .vendor_version("1.32.0")
            .await
            .unwrap_err()
            .to_string()
            .contains("run nyl vendor"));
    }

    #[tokio::test]
    async fn test_population_repairs_schemas_from_cache_without_publishing_partial_index() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("vendor");
        seed_collection(&root, "1.31.4", true);
        let index_path = root.join("schemas/builtins.json");
        let before = std::fs::read(&index_path).unwrap();
        let index = store::read_builtins(&root).unwrap();
        let cache = directory.path().join(".nyl/cache/validation-schemas");
        for (url, hash) in &index.schemas {
            let bytes = store::read_blob(&root, hash).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            store::atomic_write(
                &cache.join(format!("{}.json", store::digest(url.as_bytes()))),
                &store::json_bytes(&json!({"digest":hash,"value":value})).unwrap(),
            )
            .unwrap();
        }
        for hash in index.schemas.values() {
            let _ = std::fs::remove_file(root.join(format!("schemas/blobs/{hash}.json")));
        }
        let settings = KubeconformSettings {
            builtin_schemas: BuiltinSchemas::VendorAll,
            ..Default::default()
        };
        let mut resolver = SchemaResolver::new(directory.path(), root.clone(), &settings, true, false).unwrap();
        resolver.vendor_version("1.31.4").await.unwrap();
        assert_eq!(resolver.observed_builtins, index.schemas);
        assert_eq!(std::fs::read(&index_path).unwrap(), before);
        store::check_and_prune(&root, false).unwrap();
    }

    #[tokio::test]
    async fn test_refresh_lists_pinned_subtree_and_bypasses_saved_collection() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let subtree = "a".repeat(40);
        let responses = [
            (
                BUILTIN_REVISION.to_owned(),
                json!({"truncated":false,"tree":[{"path":"v1.31.4-standalone-strict","mode":"040000","type":"tree","sha":subtree}]}),
            ),
            (
                subtree,
                json!({"truncated":false,"tree":[{"path":"new-v1.json","mode":"100644","type":"blob","sha":"b".repeat(40)}]}),
            ),
        ];
        let server = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                for (sha, response) in responses {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let mut buffer = [0; 1024];
                        let length = socket.read(&mut buffer).await.unwrap();
                        assert!(length > 0);
                        request.extend_from_slice(&buffer[..length]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    assert!(String::from_utf8(request)
                        .unwrap()
                        .starts_with(&format!("GET /{sha} HTTP/1.1")));
                    let body = response.to_string();
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            })
            .await
            .unwrap();
        });
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("vendor");
        let saved = seed_collection(&root, "1.31.4", true);
        let settings = KubeconformSettings::default();
        let mut resolver = SchemaResolver::new(directory.path(), root, &settings, true, false).unwrap();
        resolver.registry_api = format!("http://{address}");
        assert_eq!(
            resolver
                .collection(&saved.directory, "v1.31.4-standalone-strict", BUILTIN_REVISION)
                .await
                .unwrap()
                .files,
            saved.files
        );
        resolver = resolver.with_refresh(true);
        let fresh = resolver
            .collection(&saved.directory, "v1.31.4-standalone-strict", BUILTIN_REVISION)
            .await
            .unwrap();
        assert_eq!(fresh.files, BTreeSet::from(["new-v1.json".into()]));
        server.await.unwrap();
    }

    #[test]
    fn test_directory_inventory_rejects_incomplete_and_unsafe_entries() {
        let entry = json!({"path":"secret-v1.json","mode":"100644","type":"blob","sha":"a".repeat(40)});
        for listing in [
            json!({"truncated":true,"tree":[entry.clone()]}),
            json!({"truncated":false,"tree":[]}),
            json!({"truncated":false,"tree":[entry.clone(),entry]}),
            json!({"truncated":false,"tree":[{"path":"../secret.json","mode":"100644","type":"blob","sha":"a"}]}),
            json!({"truncated":false,"tree":[{"path":"secret.json","mode":"120000","type":"blob","sha":"a"}]}),
        ] {
            assert!(
                Collection::from_tree("https://example.invalid/", serde_json::from_value(listing).unwrap()).is_err()
            );
        }
    }
}
