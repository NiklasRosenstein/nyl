//! M4 acceptance through the real binary and native tools; never silently skipped.
#![cfg(feature = "tier2")]
mod scenario;

use std::process::Command;

use scenario::shell::{project, ENVIRONMENT, NYL_TOML};
use scenario::world::{git, World};
use serde_json::Value;

fn invoke(world: &World, arguments: &[&str], exit: i32) -> Value {
    let checkout = world.fresh_clone("main");
    git(&checkout, &["config", "user.name", "Scenario"]);
    git(&checkout, &["config", "user.email", "scenario@example.invalid"]);
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = scenario::nyl_binary(&checkout, &home, arguments);
    if arguments.first() != Some(&"build") {
        command.args(["-o", "json"]);
    }
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit),
        "nyl {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid JSON: {error}: {}", String::from_utf8_lossy(&output.stdout)))
}

#[test]
fn test_native_opentofu_chain_reuses_outputs_and_native_state() {
    let output = Command::new("tofu")
        .args(["version", "-json"])
        .output()
        .expect("tier 2 requires OpenTofu on PATH");
    assert!(output.status.success(), "OpenTofu is unavailable");
    let project = project(&[
        NYL_TOML,
        ("nyl/dev.yaml", ENVIRONMENT),
        ("nyl/units.yaml", r#"apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: network, labels: {dev: 'true'}}
spec:
  source: {path: ../infra/network}
  files: ['../infra/network/**', '../infra/modules/shared/**']
  backend: {path: '%SCENARIO_TMP%/network.tfstate'}
  outputs:
    vpcId: {type: string}
---
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: database, labels: {dev: 'true'}}
spec:
  source: {path: ../infra/database}
  backend: {path: '%SCENARIO_TMP%/database.tfstate'}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}
  outputs:
    host: {type: string}
"#),
        ("infra/network/.terraform.lock.hcl", ""),
        ("infra/network/main.tf", "terraform {\n  backend \"local\" {}\n}\nmodule \"network\" {\n  source = \"../modules/shared\"\n  name = \"vpc-native\"\n}\noutput \"vpcId\" { value = module.network.id }\n"),
        ("infra/modules/shared/main.tf", "variable \"name\" { type = string }\nresource \"terraform_data\" \"network\" { input = var.name }\noutput \"id\" { value = terraform_data.network.output }\n"),
        ("infra/database/.terraform.lock.hcl", ""),
        ("infra/database/main.tf", "terraform {\n  backend \"local\" {}\n}\nvariable \"vpc_id\" { type = string }\nresource \"terraform_data\" \"database\" { input = \"db.${var.vpc_id}.internal\" }\noutput \"host\" { value = terraform_data.database.output }\n"),
    ]);
    let world = World::new(project.path());
    invoke(&world, &["state", "init", "-e", "dev"], 0);
    let first = invoke(&world, &["reconcile", "-e", "dev"], 0);
    for unit in ["network", "database"] {
        assert_eq!(first["environments"][0]["units"][unit]["result"], "executed");
    }
    let host = invoke(&world, &["get", "output", "database/host", "-e", "dev"], 0);
    assert_eq!(host, "db.vpc-native.internal");
    let native_output = Command::new("tofu")
        .arg("output")
        .arg("-json")
        .arg(format!(
            "-state={}",
            world.scenario_tmp().join("database.tfstate").display()
        ))
        .output()
        .unwrap();
    assert!(
        native_output.status.success(),
        "{}",
        String::from_utf8_lossy(&native_output.stderr)
    );
    let native: Value = serde_json::from_slice(&native_output.stdout).unwrap();
    assert_eq!(native["host"]["value"], host);
    let before = std::fs::read(world.scenario_tmp().join("network.tfstate")).unwrap();
    let repeated = invoke(&world, &["reconcile", "-e", "dev"], 0);
    assert_eq!(repeated["environments"][0]["units"]["network"]["result"], "current");
    assert_eq!(repeated["environments"][0]["units"]["database"]["result"], "current");
    assert_eq!(
        std::fs::read(world.scenario_tmp().join("network.tfstate")).unwrap(),
        before
    );
    let verified = invoke(&world, &["verify", "-e", "dev"], 0);
    assert!(verified["environments"][0]["complete"].as_bool().unwrap());
    invoke(
        &world,
        &[
            "teardown",
            "-e",
            "dev",
            "--all",
            "--reason",
            "native acceptance cleanup",
        ],
        0,
    );
    for unit in ["network", "database"] {
        let state: Value =
            serde_json::from_slice(&std::fs::read(world.scenario_tmp().join(format!("{unit}.tfstate"))).unwrap())
                .unwrap();
        assert!(state["resources"].as_array().unwrap().is_empty());
    }
}

#[test]
fn test_native_image_push_records_a_registry_digest_and_repeats_without_building() {
    let registry = scenario::native::Registry::new().unwrap();
    let units = format!("apiVersion: units.gitops.nyl/v1\nkind: OciImage\nmetadata: {{name: web, labels: {{dev: 'true'}}}}\nspec:\n  env: {{passthrough: ['%DOCKER_PASSTHROUGH%']}}\n  context: {{path: ../services/web}}\n  repository: {}/web\n", registry.address);
    let project = project(&[
        NYL_TOML,
        ("nyl/dev.yaml", ENVIRONMENT),
        ("nyl/image.yaml", &units),
        ("services/web/Dockerfile", "FROM scratch\nCOPY index.html /index.html\n"),
        ("services/web/index.html", "native acceptance\n"),
    ]);
    let world = World::with_registry(project.path(), &registry.address);
    invoke(&world, &["state", "init", "-e", "dev"], 0);
    let first = invoke(&world, &["reconcile", "-e", "dev"], 0);
    assert_eq!(first["environments"][0]["units"]["web"]["result"], "executed");
    let reference = invoke(
        &world,
        &["get", "artifact", "web/image", "-e", "dev", "--pointer", "/reference"],
        0,
    );
    let reference = reference.as_str().unwrap();
    assert!(
        reference.starts_with(&format!("{}/web@sha256:", registry.address)),
        "{reference}"
    );
    let manifest = Command::new("docker")
        .args(["buildx", "imagetools", "inspect", "--raw", reference])
        .output()
        .unwrap();
    assert!(
        manifest.status.success(),
        "{}",
        String::from_utf8_lossy(&manifest.stderr)
    );
    let manifest: Value = serde_json::from_slice(&manifest.stdout).unwrap();
    assert!(
        manifest.get("config").is_some() || manifest.get("manifests").is_some(),
        "{manifest}"
    );
    let repeated = invoke(&world, &["reconcile", "-e", "dev"], 0);
    assert_eq!(repeated["environments"][0]["units"]["web"]["result"], "current");
    invoke(&world, &["verify", "-e", "dev"], 0);
    let desired = world.rev_parse("refs/heads/nyl/dev/desired");
    let observed = world.rev_parse("refs/heads/nyl/dev/observed");
    let direct = invoke(&world, &["build", "web", "--", "--push", "--tag", "direct"], 0);
    assert_eq!(direct["spec"]["tags"], serde_json::json!(["direct"]));
    assert!(direct["spec"]["reference"].as_str().unwrap().contains("@sha256:"));
    let loaded = invoke(&world, &["build", "web", "--", "--load", "--tag", "loaded"], 0);
    assert_eq!(loaded["spec"]["tags"], serde_json::json!(["loaded"]));
    let tag = format!("{}/web:loaded", registry.address);
    let local = Command::new("docker")
        .args(["image", "inspect", &tag])
        .output()
        .unwrap();
    assert!(local.status.success(), "{}", String::from_utf8_lossy(&local.stderr));
    assert_eq!(world.rev_parse("refs/heads/nyl/dev/desired"), desired);
    assert_eq!(world.rev_parse("refs/heads/nyl/dev/observed"), observed);
    let _ = Command::new("docker").args(["image", "rm", &tag]).output();
}
