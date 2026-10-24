//! The dependency direction of the Nyl crates, from
//! `design/implementation-architecture.md#crates`. Cargo enforces whatever
//! the manifests declare; this test keeps the manifests to the direction the
//! architecture allows, so orchestration can never reach Git, processes, or
//! rendering except through the traits `nyl-core` owns.

use std::collections::BTreeSet;
use std::path::Path;

/// Each crate and the Nyl crates it may depend on.
const ALLOWED: [(&str, &[&str]); 7] = [
    ("crates/nyl-core", &[]),
    ("crates/nyl-git", &[]),
    ("crates/nyl-render", &["nyl-core", "nyl-git"]),
    ("crates/nyl-state", &["nyl-core", "nyl-git"]),
    ("crates/nyl-orchestration", &["nyl-core"]),
    ("crates/nyl-drivers", &["nyl-core", "nyl-render"]),
    (
        "nyl",
        &[
            "nyl-core",
            "nyl-git",
            "nyl-render",
            "nyl-state",
            "nyl-orchestration",
            "nyl-drivers",
        ],
    ),
];

#[test]
fn test_crates_depend_only_in_the_architecture_direction() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (directory, allowed) in ALLOWED {
        let manifest: toml::Table = std::fs::read_to_string(root.join(directory).join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        let nyl_dependencies = ["dependencies", "dev-dependencies", "build-dependencies"]
            .iter()
            .filter_map(|section| manifest.get(*section).and_then(toml::Value::as_table))
            .flat_map(|table| table.keys())
            .filter(|name| name.starts_with("nyl"))
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
        let forbidden = nyl_dependencies.difference(&allowed).collect::<Vec<_>>();
        assert!(forbidden.is_empty(), "{directory} must not depend on {forbidden:?}");
    }
}
