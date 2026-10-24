//! The drivers an invocation dispatches to.
//!
//! The command line wires [`builtin`] in production. Tier-1 scenarios and a
//! binary built with the `test-kinds` feature use [`with_test_kinds`], which
//! registers each fake kind under the built-in kind it stands in for, so
//! state documents keep the real `apiVersion` and `kind`.

use std::sync::Arc;

use nyl_core::orchestration::driver::DriverSet;

use crate::command::CommandDriver;

/// The built-in kinds of this milestone: `Command`.
pub fn builtin() -> DriverSet {
    let mut drivers = DriverSet::new();
    drivers.register(Arc::new(CommandDriver));
    drivers
}

/// The built-in kinds plus the fakes standing in for the kinds this
/// milestone lacks (`OciImage`, `OpenTofu`, `Terraform`,
/// `KubernetesPublication`), all sharing one `script`.
#[cfg(any(test, feature = "test-kinds"))]
pub fn with_test_kinds(script: &crate::fake::Script) -> DriverSet {
    use crate::fake::{stand_in_for, FakeImage, FakeInfra, FakePublication};
    use nyl_core::constants::API_VERSION_UNITS;
    use nyl_core::orchestration::driver::Driver;

    let mut drivers = builtin();
    let fakes: Vec<Arc<dyn Driver>> = vec![
        Arc::new(FakeImage::new(script.clone())),
        Arc::new(FakeInfra::new(script.clone())),
        Arc::new(FakePublication::new(script.clone())),
    ];
    for fake in fakes {
        drivers.register(fake.clone());
        for kind in ["OciImage", "OpenTofu", "Terraform", "KubernetesPublication"] {
            if stand_in_for(API_VERSION_UNITS, kind) == Some(fake.kind()) {
                drivers.register_as(API_VERSION_UNITS, kind, fake.clone());
            }
        }
    }
    drivers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fakes_serve_the_real_kinds_they_stand_in_for() {
        let drivers = with_test_kinds(&crate::fake::Script::new());
        let kind = |real: &str| {
            drivers
                .get("units.gitops.nyl/v1", real)
                .map(nyl_core::orchestration::driver::Driver::kind)
        };
        assert_eq!(kind("Command"), Some("Command"));
        assert_eq!(kind("OciImage"), Some("FakeImage"));
        assert_eq!(kind("OpenTofu"), Some("FakeInfra"));
        assert_eq!(kind("KubernetesPublication"), Some("FakePublication"));
        assert!(builtin().get("units.gitops.nyl/v1", "OciImage").is_none());
    }
}
