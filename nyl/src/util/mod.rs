/// Utility functions
///
/// This module will handle:
/// - Hash computation
/// - File system utilities
/// - Process execution helpers
use sha2::{Digest, Sha256};

pub mod fs;
pub use fs::{find_config_file, resolve_path, resolve_paths};

/// Compute SHA256 hash of a string
pub fn compute_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Execute a command and return output
pub fn execute_command(_cmd: &str, _args: &[&str]) -> crate::Result<String> {
    // Stub implementation
    Ok(String::new())
}
