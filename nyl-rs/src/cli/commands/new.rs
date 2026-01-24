use clap::Args;

use crate::Result;

/// Create a new nyl project
#[derive(Args, Debug)]
pub struct NewArgs {
    /// Name of the new project
    pub name: String,

    /// Path where to create the project
    #[arg(short, long, default_value = ".")]
    pub path: String,

    /// Use a template
    #[arg(short, long)]
    pub template: Option<String>,
}

pub fn execute(_args: NewArgs) -> Result<()> {
    println!("new command: not yet implemented");
    Ok(())
}
