//! The declared types of the Release inputs a publication unit's
//! `$fromPromotion` bindings fill, for promotion validation
//! ([`Driver::bound_field_types`](nyl_core::orchestration::driver::Driver::bound_field_types)).
//!
//! The renderer copies the target's `releaseInputs` into the unit's spec
//! under `releases`, so a binding of input `<input>` of Release
//! `<group>/<release>` sits at `/releases/<group>~1<release>/<input>`. The
//! kind's schema cannot type it: the Release declares it, in an
//! ApplicationGroup's source. This module reads the declarations of the
//! Releases the target selects without rendering
//! ([`nyl_render::gitops::publication::declared_release_inputs`]).
//!
//! Contract: [PromotionPath](../../../../design/promotion.md#promotionpath),
//! "Validation checks every path", and [Declarations](../../../../design/release-inputs.md#declarations).

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::{BoundFieldType, DeploymentContext, DriverError, DriverResult};
use nyl_render::gitops::units::PublicationTarget;
use nyl_render::resources::release_inputs::ReleaseKey;
use serde_json::Value;

/// The Release and input a pointer into the spec names, when it names one:
/// `/releases/<group>~1<release>/<input>`.
fn release_input(pointer: &str) -> Option<(ReleaseKey, String)> {
    let mut tokens = pointer.strip_prefix("/releases/")?.split('/');
    let (Some(key), Some(input), None) = (tokens.next(), tokens.next(), tokens.next()) else {
        return None;
    };
    let unescape = |token: &str| token.replace("~1", "/").replace("~0", "~");
    Some((ReleaseKey::parse(&unescape(key)).ok()?, unescape(input)))
}

/// The declared type of each Release input at `pointers`, read from the
/// Releases the unit's target selects. An input of a Release no enabled
/// selected group holds, or one its Release does not declare, is left out:
/// resolution reports it. A group whose source cannot be read leaves its
/// inputs [`BoundFieldType::Unchecked`].
pub(crate) fn bound_field_types(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &DeploymentContext<'_>,
    spec: &Value,
    pointers: &[String],
) -> DriverResult<BTreeMap<String, BoundFieldType>> {
    let wanted: Vec<(&String, ReleaseKey, String)> = pointers
        .iter()
        .filter_map(|pointer| release_input(pointer).map(|(key, input)| (pointer, key, input)))
        .collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let target = super::run::publication_target(spec)?;
    let inline = match &target {
        PublicationTarget::Inline(inline) => Some(inline),
        PublicationTarget::Static(_) => None,
    };
    let declared = nyl_render::gitops::publication::declared_release_inputs(
        &nyl_render::gitops::publication::DeclaredInputsRequest {
            worktree: ctx.worktree,
            project_dir: std::path::Path::new(ctx.project_dir),
            target: target.name(),
            inline,
            cache_dir: ctx.cache_dir,
            source_cache_dir: ctx.source_cache_dir,
            offline: ctx.offline,
            resource_catalogue: catalogue.clone(),
            instances: ctx.instances,
        },
    )
    .map_err(|error| {
        DriverError::before_effects(format!(
            "cannot read the Release input declarations of DeploymentTarget {:?}: {error}",
            target.name()
        ))
    })?;
    let mut types = BTreeMap::new();
    for (pointer, key, input) in wanted {
        let found = match declared.get(&key.group) {
            None => None,
            Some(Err(reason)) => Some(BoundFieldType::Unchecked(format!(
                "the source of ApplicationGroup {:?}, which declares Release {key}, cannot be read: {reason}",
                key.group
            ))),
            Some(Ok(releases)) => releases
                .get(&key.release)
                .and_then(|inputs| inputs.get(&input))
                .map(|declaration| BoundFieldType::Typed(declaration.input_type)),
        };
        if let Some(found) = found {
            types.insert(pointer.clone(), found);
        }
    }
    Ok(types)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a binding beside an input, or inside one, being taken for the input itself.
    #[test]
    fn test_only_an_input_of_a_release_is_a_release_input() {
        let (key, input) = release_input("/releases/web~1api/image").unwrap();
        assert_eq!(
            (key.group.as_str(), key.release.as_str(), input.as_str()),
            ("web", "api", "image")
        );
        assert!(release_input("/releases/web~1api").is_none());
        assert!(release_input("/releases/web~1api/image/tag").is_none());
        assert!(release_input("/target").is_none());
    }
}
