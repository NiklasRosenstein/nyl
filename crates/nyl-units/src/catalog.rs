//! Built-in unit schema descriptors.
use nyl_core::constants::API_VERSION_UNITS;
use nyl_core::resources::schema::{set_envelope, ResourceDescriptor};
use schemars::schema_for;
use serde_json::{json, Value};

fn schema(name: &str, mut schema: Value, spec: &Value) -> Value {
    set_envelope(&mut schema, API_VERSION_UNITS, Some(name));
    schema["title"] = json!(name);
    if schema.get("examples").is_none() {
        schema["examples"] =
            json!([{ "apiVersion": API_VERSION_UNITS, "kind": name, "metadata": {"name": "workloads"}, "spec": spec }]);
    }
    schema
}
fn image() -> Value {
    schema(
        "OciImage",
        json!(schema_for!(crate::oci_image::OciImage)),
        &json!({"context":{"path":"services/web"},"repository":"registry.example.com/team/web","platforms":["linux/amd64"]}),
    )
}
fn infrastructure(name: &str) -> Value {
    schema(
        name,
        json!(schema_for!(crate::terraform::TerraformUnit)),
        &json!({"source":{"path":"infra/network"},"backend":{"bucket":"acme-state","key":"production/network"},"variables":{"region":"eu-west-1"},"outputs":{"id":{"type":"string"}}}),
    )
}
fn terraform() -> Value {
    infrastructure("Terraform")
}
fn tofu() -> Value {
    infrastructure("OpenTofu")
}
/// Unit resources with published schemas, in catalogue order.
pub const RESOURCES: &[ResourceDescriptor] = &[
    ResourceDescriptor {
        name: "OciImage",
        api_version: API_VERSION_UNITS,
        slug: "oci-image",
        aliases: &[],
        dynamic_kind: false,
        schema: image,
    },
    ResourceDescriptor {
        name: "Terraform",
        api_version: API_VERSION_UNITS,
        slug: "terraform",
        aliases: &[],
        dynamic_kind: false,
        schema: terraform,
    },
    ResourceDescriptor {
        name: "OpenTofu",
        api_version: API_VERSION_UNITS,
        slug: "open-tofu",
        aliases: &["tofu"],
        dynamic_kind: false,
        schema: tofu,
    },
];
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_examples_validate() {
        for descriptor in RESOURCES {
            let schema = (descriptor.schema)();
            let example = schema["examples"][0].clone();
            assert_eq!(example["apiVersion"], descriptor.api_version);
            assert_eq!(example["kind"], descriptor.name);
            match descriptor.name {
                "OciImage" => {
                    serde_json::from_value::<crate::oci_image::OciImage>(example)
                        .unwrap()
                        .spec
                        .validate()
                        .unwrap();
                }
                "Terraform" | "OpenTofu" => {
                    serde_json::from_value::<crate::terraform::TerraformUnit>(example)
                        .unwrap()
                        .spec
                        .validate()
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
    }
}
