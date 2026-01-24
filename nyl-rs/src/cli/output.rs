/// Output formatting utilities
use std::io::{self, Write};

/// Print a message to stdout
pub fn print_message(msg: &str) {
    println!("{msg}");
}

/// Print an error message to stderr
pub fn print_error(msg: &str) {
    eprintln!("Error: {msg}");
}

/// Print YAML content to stdout
pub fn print_yaml(content: &str) -> io::Result<()> {
    io::stdout().write_all(content.as_bytes())?;
    io::stdout().flush()
}
