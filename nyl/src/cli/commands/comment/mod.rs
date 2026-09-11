//! Sticky forge comments, independent of manifest rendering and project configuration.

mod context;
mod provider;

use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::{NylError, Result};
use context::{Context, Provider};

/// Manage pull request and merge request comments.
#[derive(Debug, Args)]
pub struct CommentArgs {
    #[command(subcommand)]
    command: CommentCommand,
}

#[derive(Debug, Subcommand)]
enum CommentCommand {
    /// Create or update a comment owned by the authenticated account
    Upsert(UpsertArgs),
}

#[derive(Debug, Args)]
struct UpsertArgs {
    /// Stable comment key; different keys maintain separate comments
    #[arg(long)]
    key: String,
    /// UTF-8 Markdown file, or - to read stdin
    #[arg(long)]
    body_file: PathBuf,
    /// Forge provider (otherwise detected from CI)
    #[arg(long, value_enum)]
    provider: Option<Provider>,
    /// Forge web URL, including any self-hosted path prefix (not an API URL)
    #[arg(long)]
    server_url: Option<String>,
    /// owner/repo, GitLab group/project path, or GitLab numeric project ID
    #[arg(long)]
    repository: Option<String>,
    /// Pull request number or merge request IID
    #[arg(long)]
    request: Option<u64>,
    /// Read remote state and print the proposed outcome without writing a comment
    #[arg(long)]
    dry_run: bool,
}

/// Execute a comment operation without loading a Nyl project.
pub async fn execute(args: CommentArgs) -> Result<()> {
    let CommentCommand::Upsert(args) = args.command;
    let context = Context::resolve(&args, |name| std::env::var(name).ok())?;
    let marker = marker(&args.key)?;
    let input: Box<dyn Read> = if args.body_file.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(
            std::fs::File::open(&args.body_file)
                .map_err(|_| error("Cannot open --body-file. Check that the Markdown file exists and is readable."))?,
        )
    };
    let body = read_body(input, &marker, context.provider.body_limit())?;
    let token = context.provider.token(|name| std::env::var(name).ok())?;
    let result = provider::upsert(context, token, &marker, &body, args.dry_run).await?;
    if args.dry_run {
        println!("dry-run {} {}", result.outcome, result.url);
    } else {
        println!("{} {}", result.outcome, result.url);
    }
    Ok(())
}

fn error(message: impl Into<String>) -> NylError {
    NylError::Comment(message.into())
}

fn marker(key: &str) -> Result<String> {
    if key.trim().is_empty() || key.len() > 256 {
        return Err(error("--key must contain 1–256 UTF-8 bytes and cannot be blank."));
    }
    Ok(format!("<!-- nyl-comment:v1:{} -->", hex::encode(key)))
}

fn read_body(input: impl Read, marker: &str, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error("Cannot read --body-file. Check file permissions or the stdin producer."))?;
    if bytes.len() + marker.len() + 2 > limit {
        return Err(error(format!(
            "Comment exceeds the {limit}-byte limit including its marker. Shorten the report and link to a CI artifact."
        )));
    }
    let markdown = String::from_utf8(bytes).map_err(|_| error("--body-file must contain UTF-8 Markdown."))?;
    if markdown.contains("<!-- nyl-comment:") {
        return Err(error(
            "The report contains a reserved nyl-comment marker. Supply Markdown without a sticky-comment marker.",
        ));
    }
    Ok(format!("{marker}\n\n{markdown}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_marker_keys_are_distinct_and_cannot_escape_html_comments() {
        assert_eq!(
            marker("gitops/kasoku").unwrap(),
            "<!-- nyl-comment:v1:6769746f70732f6b61736f6b75 -->"
        );
        assert_ne!(marker("a").unwrap(), marker("a ").unwrap());
        assert_eq!(marker("-->\n").unwrap(), "<!-- nyl-comment:v1:2d2d3e0a -->");
        assert!(marker(" ").is_err());
    }

    #[test]
    fn test_bounded_report_fits_every_provider_with_maximum_comment_key() {
        let marker = marker(&"k".repeat(256)).unwrap();
        let body = "界".repeat(20_000);
        for provider in [Provider::Github, Provider::Gitlab, Provider::Forgejo] {
            assert_eq!(
                read_body(body.as_bytes(), &marker, provider.body_limit()).unwrap(),
                format!("{marker}\n\n{body}")
            );
        }
    }

    #[test]
    fn test_read_body_preserves_markdown_and_checks_complete_payload_size() {
        let marker = marker("key").unwrap();
        let input = "# Report\r\n\né\n";
        let limit = marker.len() + 2 + input.len();
        assert_eq!(
            read_body(input.as_bytes(), &marker, limit).unwrap(),
            format!("{marker}\n\n{input}")
        );
        assert!(read_body(input.as_bytes(), &marker, limit - 1).is_err());
        assert!(read_body(&b"\xff"[..], &marker, 100).is_err());
        assert!(read_body(marker.as_bytes(), &marker, 1000).is_err());
    }
}
