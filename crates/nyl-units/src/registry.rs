//! The drivers an invocation dispatches to.
//!
//! The command line wires [`builtin`] in production. Tier-1 scenarios and a
//! binary built with the `test-kinds` feature use [`with_test_kinds`], which
//! registers each fake kind under the built-in kind it stands in for, so
//! state documents keep the real `apiVersion` and `kind`. Tier-1 keeps
//! `FakePublication` because scenarios script a publication's outcome and
//! answer `observe` with the fake Argo CD observer.

use std::sync::Arc;

use nyl_core::orchestration::driver::DriverSet;

use crate::command::CommandDriver;

/// Every built-in driver: commands, images, infrastructure, and Kubernetes
/// publications, which render against `catalogue`, the resource catalogue
/// the command line passes to every other workflow.
pub fn builtin(catalogue: nyl_render::config::ResourceCatalogue) -> DriverSet {
    let mut drivers = DriverSet::new();
    drivers.register(Arc::new(CommandDriver));
    drivers.register(Arc::new(
        crate::kubernetes_publication::KubernetesPublicationDriver::new(catalogue),
    ));
    drivers.register(Arc::new(crate::oci_image::OciImageDriver));
    drivers.register(Arc::new(crate::terraform::TerraformDriver::terraform()));
    drivers.register(Arc::new(crate::terraform::TerraformDriver::opentofu()));
    drivers
}

/// [`with_test_kinds`] with the real `KubernetesPublication` driver instead of
/// `FakePublication`: fake images and infrastructure around the driver that
/// publishes to Git. Its observed values are read through the script
/// ([`ScriptedLive`](crate::fake::ScriptedLive)) instead of Argo CD and the
/// workload Cluster, which the stub-based driver tests cover; everything else
/// of the driver, such as resolving and publishing, is the built-in one.
#[cfg(any(test, feature = "test-kinds"))]
pub fn with_real_publication(
    script: &crate::fake::Script,
    catalogue: nyl_render::config::ResourceCatalogue,
) -> DriverSet {
    use crate::fake::{FakeImage, FakeInfra};
    use nyl_core::constants::API_VERSION_UNITS;
    use nyl_core::orchestration::driver::Driver;

    let mut drivers = builtin(catalogue.clone());
    drivers.register(Arc::new(
        crate::kubernetes_publication::KubernetesPublicationDriver::new(catalogue)
            .with_live_reader(Arc::new(crate::fake::ScriptedLive::new(script.clone()))),
    ));
    let image: Arc<dyn Driver> = Arc::new(FakeImage::new(script.clone()));
    let infra: Arc<dyn Driver> = Arc::new(FakeInfra::new(script.clone()));
    for (kind, fake) in [("OciImage", &image), ("OpenTofu", &infra), ("Terraform", &infra)] {
        drivers.register_as(API_VERSION_UNITS, kind, fake.clone());
    }
    for fake in [image, infra] {
        drivers.register(fake);
    }
    drivers
}

/// The built-in kinds plus the fakes that tier-1 scenarios and tests script
/// (`OciImage`, `OpenTofu`, `Terraform`, `KubernetesPublication`), all
/// sharing one `script`. The fake publication renders against `catalogue`.
#[cfg(any(test, feature = "test-kinds"))]
pub fn with_test_kinds(script: &crate::fake::Script, catalogue: nyl_render::config::ResourceCatalogue) -> DriverSet {
    use crate::fake::{FakeImage, FakeInfra, FakePublication};
    use nyl_core::constants::API_VERSION_UNITS;
    use nyl_core::orchestration::driver::Driver;

    let mut drivers = builtin(catalogue.clone());
    let image: Arc<dyn Driver> = Arc::new(FakeImage::new(script.clone()));
    let infra: Arc<dyn Driver> = Arc::new(FakeInfra::new(script.clone()));
    let publication: Arc<dyn Driver> = Arc::new(FakePublication::new(script.clone(), catalogue));
    for (kind, fake) in [
        ("OciImage", &image),
        ("OpenTofu", &infra),
        ("Terraform", &infra),
        ("KubernetesPublication", &publication),
    ] {
        drivers.register_as(API_VERSION_UNITS, kind, fake.clone());
    }
    for fake in [image, infra, publication] {
        drivers.register(fake);
    }
    drivers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fakes_serve_the_real_kinds_they_stand_in_for() {
        let drivers = with_test_kinds(&crate::fake::Script::new(), crate::fake::test_catalogue());
        let kind = |real: &str| {
            drivers
                .get("units.gitops.nyl/v1", real)
                .map(nyl_core::orchestration::driver::Driver::kind)
        };
        assert_eq!(kind("Command"), Some("Command"));
        assert_eq!(kind("OciImage"), Some("FakeImage"));
        assert_eq!(kind("OpenTofu"), Some("FakeInfra"));
        assert_eq!(kind("KubernetesPublication"), Some("FakePublication"));
        // The real publication stays registered under its own kind, and
        // `with_real_publication` keeps it for the built-in kind.
        let real = with_real_publication(&crate::fake::Script::new(), crate::fake::test_catalogue());
        let real_kind = |name: &str| {
            real.get("units.gitops.nyl/v1", name)
                .map(nyl_core::orchestration::driver::Driver::kind)
        };
        assert_eq!(real_kind("KubernetesPublication"), Some("KubernetesPublication"));
        assert_eq!(real_kind("OciImage"), Some("FakeImage"));
        // Every built-in kind has a driver, so a release build serves each.
        let builtin = builtin(crate::fake::test_catalogue());
        for kind in ["Command", "OciImage", "OpenTofu", "Terraform", "KubernetesPublication"] {
            assert!(builtin.get("units.gitops.nyl/v1", kind).is_some(), "{kind}");
        }
    }
}
