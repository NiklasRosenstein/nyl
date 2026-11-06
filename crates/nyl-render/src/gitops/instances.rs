//! Instances of EnvironmentTemplates, as rendering sees them.
//!
//! Contract: [Environment templates and previews](../../../../design/orchestration-core.md#environment-templates-and-previews).
//!
//! An instance is a template plus recorded parameters; this module turns the
//! pair into the Environment every other entry point already understands, so
//! a command never needs to know whether an environment was declared or
//! instantiated:
//!
//! - every string of the template's `values`, `state`, `source`, and
//!   `protectedRefs` that has template syntax renders with `params` and
//!   `environment.name`; the result is an ordinary [`Environment`], validated
//!   like a declared one, whose labels are the template's, so groups apply;
//! - `state` renders with `environment.name` alone, because it locates the
//!   `state.yaml` that holds the parameters;
//! - the template's lifecycle fields travel as [`InstanceTerms`];
//! - validation renders [`SAMPLE_INSTANCES`] sample instances of each
//!   template, so a template whose generated names collide across instances
//!   fails without any instance existing ([`sample_instances`]).
//!
//! Which instances exist is state, so the inventory only carries the ones its
//! caller read ([`GitOpsInventory::instances`]).

use std::collections::BTreeMap;

use nyl_core::bindings::InputType;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::instance::{InstanceDescriptor, InstanceTerms};
use nyl_core::resources::environment::{
    effective_environment, Environment, EnvironmentGroup, EnvironmentState, EnvironmentTemplate,
};
use nyl_core::template_syntax::has_template_syntax;
use serde_json::{json, Map, Value};

use super::GitOpsInventory;
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::template::TemplateEngine;
use crate::{NylError, Result};

/// How many instances of a template validation renders.
pub const SAMPLE_INSTANCES: usize = 2;

/// An environment as commands see it: its effective spec, and for an
/// instance the template's terms with its parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveEnvironment {
    /// The Environment with group defaults applied.
    pub environment: Environment,
    /// Set for an instance.
    pub instance: Option<InstanceTerms>,
}

impl EffectiveEnvironment {
    /// The parameters templates read as `params`; `None` for a declared environment.
    pub fn params(&self) -> Option<&BTreeMap<String, Value>> {
        self.instance.as_ref().map(|terms| &terms.params)
    }
}

/// The EnvironmentTemplates the inventory declares.
pub fn templates(inventory: &GitOpsInventory) -> Vec<&EnvironmentTemplate> {
    inventory
        .resources
        .values()
        .filter_map(|discovered| match &discovered.resource {
            Some(GitOpsResource::EnvironmentTemplate(template)) => Some(template.as_ref()),
            _ => None,
        })
        .collect()
}

/// The EnvironmentTemplate `name`.
pub fn find_template<'a>(inventory: &'a GitOpsInventory, name: &str) -> Option<&'a EnvironmentTemplate> {
    match inventory
        .get(GitOpsResourceKind::EnvironmentTemplate, name)?
        .resource
        .as_ref()?
    {
        GitOpsResource::EnvironmentTemplate(template) => Some(template.as_ref()),
        _ => None,
    }
}

/// The finding of a template that does not render to an Environment.
fn invalid(template: &EnvironmentTemplate, name: &str, message: impl std::fmt::Display) -> Finding {
    Finding::new(
        Code::TemplateInvalid,
        format!(
            "EnvironmentTemplate {} does not render to a valid environment {name}: {message}",
            template.metadata.name
        ),
        format!("fix EnvironmentTemplate {} in source", template.metadata.name),
    )
}

/// Render every string of `value` that has template syntax.
fn render_strings(engine: &TemplateEngine, value: &mut Value, context: &Value) -> crate::Result<()> {
    match value {
        Value::String(text) if has_template_syntax(text) => {
            *text = engine.render(text, context)?;
        }
        Value::Array(items) => {
            for item in items {
                render_strings(engine, item, context)?;
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                render_strings(engine, item, context)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The state location settings of the instance `name`, rendered with
/// `environment.name` only. This is what finds the instance's `state.yaml`,
/// so it cannot read parameters.
pub fn instance_state(
    template: &EnvironmentTemplate,
    name: &EnvironmentName,
) -> std::result::Result<EnvironmentState, Finding> {
    let mut value =
        serde_json::to_value(&template.spec.state).map_err(|error| invalid(template, name.as_str(), error))?;
    let context = json!({"environment": {"name": name.as_str()}});
    render_strings(&TemplateEngine::new(), &mut value, &context).map_err(|error| {
        invalid(
            template,
            name.as_str(),
            format!("spec.state may only read environment.name: {error}"),
        )
    })?;
    serde_json::from_value(value).map_err(|error| invalid(template, name.as_str(), error))
}

/// The Environment the instance `name` of `template` renders to with
/// `params`, and the template's terms.
pub fn instantiate(
    template: &EnvironmentTemplate,
    name: &EnvironmentName,
    params: &BTreeMap<String, Value>,
) -> std::result::Result<(Environment, InstanceTerms), Finding> {
    let bound = template.bind_parameters(params).map_err(|message| {
        Finding::new(
            Code::InstanceParameterInvalid,
            message,
            format!(
                "pass parameters as --param <name>=<value>, declared by EnvironmentTemplate {}",
                template.metadata.name
            ),
        )
    })?;
    let skeleton = template.environment_skeleton(name.as_str());
    let mut spec = serde_json::to_value(&skeleton.spec).map_err(|error| invalid(template, name.as_str(), error))?;
    let context = json!({
        "params": bound,
        "environment": {"name": name.as_str(), "labels": template.metadata.labels},
    });
    let engine = TemplateEngine::new();
    if let Value::Object(map) = &mut spec {
        for field in ["values", "source", "protectedRefs"] {
            if let Some(value) = map.get_mut(field) {
                render_strings(&engine, value, &context)
                    .map_err(|error| invalid(template, name.as_str(), format!("spec.{field}: {error}")))?;
            }
        }
        // `state` reads only `environment.name`; its rendering is the one
        // that located the state.
        map.insert(
            "state".to_owned(),
            serde_json::to_value(instance_state(template, name)?)
                .map_err(|error| invalid(template, name.as_str(), error))?,
        );
    }
    let mut environment = skeleton;
    environment.spec = serde_json::from_value(spec).map_err(|error| invalid(template, name.as_str(), error))?;
    environment
        .validate()
        .map_err(|error| invalid(template, name.as_str(), error))?;
    let terms = InstanceTerms {
        template: template.metadata.name.clone(),
        params: bound,
        deletion_policy: template.spec.deletion_policy,
        allow_teardown: template.spec.allow_teardown,
        ttl: template.spec.ttl,
        max_instances: template.spec.max_instances,
    };
    Ok((environment, terms))
}

/// The declared Environments and EnvironmentGroups of the inventory.
fn declared(inventory: &GitOpsInventory) -> (Vec<&Environment>, Vec<EnvironmentGroup>) {
    let mut environments = Vec::new();
    let mut groups = Vec::new();
    for discovered in inventory.resources.values() {
        match &discovered.resource {
            Some(GitOpsResource::Environment(environment)) => environments.push(environment.as_ref()),
            Some(GitOpsResource::EnvironmentGroup(group)) => groups.push(group.clone()),
            _ => {}
        }
    }
    (environments, groups)
}

/// The names of every environment the inventory knows: declared, then instances.
pub fn environment_names(inventory: &GitOpsInventory) -> Result<Vec<EnvironmentName>> {
    let mut names = Vec::new();
    for environment in declared(inventory).0 {
        names.push(environment.name()?);
    }
    names.extend(inventory.instances.keys().cloned());
    Ok(names)
}

/// The environment `name` with its group defaults applied, through the one
/// group rule of nyl-core: a declared Environment, else an instance the
/// inventory knows.
pub fn resolve_environment(inventory: &GitOpsInventory, name: &EnvironmentName) -> Result<EffectiveEnvironment> {
    let (environments, groups) = declared(inventory);
    if let Some(environment) = environments
        .into_iter()
        .find(|environment| environment.metadata.name == name.as_str())
    {
        let environment =
            effective_environment(environment, &groups).map_err(|finding| NylError::config(finding.to_string()))?;
        return Ok(EffectiveEnvironment {
            environment,
            instance: None,
        });
    }
    if let Some(InstanceDescriptor { template, params }) = inventory.instances.get(name) {
        let declaration = find_template(inventory, template).ok_or_else(|| {
            NylError::config(format!(
                "{}: environment {name} is an instance of EnvironmentTemplate {template}, which this project does not declare",
                Code::InstanceTemplateUnknown
            ))
        })?;
        let (environment, terms) =
            instantiate(declaration, name, params).map_err(|finding| NylError::config(finding.to_string()))?;
        let environment =
            effective_environment(&environment, &groups).map_err(|finding| NylError::config(finding.to_string()))?;
        return Ok(EffectiveEnvironment {
            environment,
            instance: Some(terms),
        });
    }
    Err(NylError::config(format!(
        "{}: Environment {name} is not declared in this project, and no known instance has that name",
        Code::EnvironmentUnknown
    )))
}

/// Sample instances of `template` for validation: [`SAMPLE_INSTANCES`]
/// instances with distinct names and distinct parameter values, each
/// parameter taking its declaration's `enum` values in turn, else a value of
/// its type that differs between samples.
pub fn sample_instances(template: &EnvironmentTemplate) -> Vec<(EnvironmentName, InstanceDescriptor)> {
    (0..SAMPLE_INSTANCES)
        .map(|index| {
            let params = template
                .spec
                .parameters
                .iter()
                .map(|(name, declaration)| {
                    let value = match &declaration.allowed {
                        Some(allowed) if !allowed.is_empty() => allowed[index % allowed.len()].clone(),
                        _ => sample_value(declaration.input_type, index),
                    };
                    (name.clone(), value)
                })
                .collect();
            let name = EnvironmentName::new(format!("{}-sample-{}", template.metadata.name, index + 1))
                .or_else(|_| EnvironmentName::new(format!("sample-{}", index + 1)))
                .expect("a sample name is a valid environment name");
            (
                name,
                InstanceDescriptor {
                    template: template.metadata.name.clone(),
                    params,
                },
            )
        })
        .collect()
}

fn sample_value(input_type: InputType, index: usize) -> Value {
    match input_type {
        InputType::String => json!(format!("sample{}", index + 1)),
        InputType::Integer | InputType::Number => json!(index + 1),
        InputType::Boolean => json!(index % 2 == 0),
        InputType::Object => Value::Object(Map::new()),
        InputType::Array => json!([]),
    }
}

impl GitOpsInventory {
    /// The inventory with `instances` as known instances, which every
    /// environment-wide function (rendering, placements, inline targets,
    /// Argo CD name checks) then includes.
    #[must_use]
    pub fn with_instances(mut self, instances: BTreeMap<EnvironmentName, InstanceDescriptor>) -> Self {
        self.instances.extend(instances);
        self
    }
}
