//! What every step of the scenario works with: the infrastructure, `kubectl`,
//! the project checkout, and the `nyl` runner with its admitted credentials.

use std::path::Path;

use crate::error::{Context, Result};
use crate::infra::Infra;
use crate::kubectl::Kubectl;
use crate::nyl::{docker_selectors, Nyl};
use crate::project::Project;
use crate::reconciler::Reconciler;

pub struct Env {
    pub infra: Infra,
    pub kubectl: Kubectl,
    pub project: Project,
    pub nyl: Nyl,
    /// The directory OpenTofu keeps its local state in, outside the checkout.
    pub state_dir: String,
    /// The Docker client variables the image unit admits: the ones `nyl` was
    /// given, which the unit must name because an admitted variable must exist.
    pub docker_env: Vec<String>,
}

impl Env {
    /// A fresh checkout and state directory in the infrastructure's directory,
    /// and a `nyl` runner that admits the cluster's kubeconfig, the Docker
    /// client variables, and whatever credentials the reconciler needs.
    pub fn new(infra: Infra, nyl_binary: &Path, reconciler: &dyn Reconciler, search_path: String) -> Result<Self> {
        let directory = infra.directory.clone();
        let project = Project::init(&directory.join("clone"), &infra.git_publish_url())?;
        let home = directory.join("home");
        let state = directory.join("tofu");
        for path in [&home, &state] {
            if path.exists() {
                std::fs::remove_dir_all(path).context(format!("remove {}", path.display()))?;
            }
            std::fs::create_dir_all(path).context(format!("create {}", path.display()))?;
        }
        let binary = nyl_binary
            .canonicalize()
            .context(format!("find the nyl binary {}", nyl_binary.display()))?;
        let mut nyl = Nyl::new(binary, project.directory().to_owned(), home, search_path);
        nyl.admit("KUBECONFIG", infra.kubeconfig.to_string_lossy());
        let selectors = docker_selectors(
            |name| std::env::var(name).ok(),
            std::env::var_os("HOME").as_deref().map(Path::new),
            Path::is_dir,
        );
        let docker_env = selectors.keys().cloned().collect();
        for (name, value) in selectors {
            nyl.admit(name, value);
        }
        for (name, value) in reconciler.credentials(&infra)? {
            nyl.admit(name, value);
        }
        let kubectl = infra.kubectl();
        Ok(Self {
            infra,
            kubectl,
            project,
            nyl,
            state_dir: forward_slashes(&state),
            docker_env,
        })
    }
}

/// A path as the fixture writes it: forward slashes, so a unit's script and a
/// YAML string read it the same on every platform.
pub fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_are_written_with_forward_slashes() {
        assert_eq!(forward_slashes(Path::new("C:\\work\\tofu")), "C:/work/tofu");
        assert_eq!(forward_slashes(Path::new("/work/tofu")), "/work/tofu");
    }
}
