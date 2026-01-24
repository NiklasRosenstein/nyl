use clap::Args;

use crate::Result;

/// Show diff between rendered manifests and cluster state
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Component to diff (if not specified, diffs all)
    #[arg(short, long)]
    pub component: Option<String>,

    /// Environment to diff for
    #[arg(short, long)]
    pub environment: Option<String>,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

pub fn execute(_args: DiffArgs) -> Result<()> {
    println!("diff command: not yet implemented");
    Ok(())
}
