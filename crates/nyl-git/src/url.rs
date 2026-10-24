//! Keeping credentials embedded in URLs out of messages.

/// Sanitize a URL by redacting userinfo (username/password/tokens)
///
/// This removes credentials from URLs to prevent them from appearing in logs.
/// Examples:
/// - `https://user:token@github.com/repo` -> `https://***@github.com/repo`
/// - `oci://user:pass@registry.io/chart` -> `oci://***@registry.io/chart`
/// - `https://github.com/repo` -> `https://github.com/repo` (unchanged)
pub fn sanitize_url(url: &str) -> String {
    if let Some(proto_end) = url.find("://") {
        let proto_end = proto_end + 3;
        let after_proto = &url[proto_end..];
        if let Some(at_pos) = after_proto.find('@') {
            let proto = &url[..proto_end];
            let host_and_path = &after_proto[at_pos..];
            return format!("{proto}***{host_and_path}");
        }
    }
    url.to_string()
}

/// Replace every occurrence of `url` in `text` with its [`sanitize_url`] form,
/// for messages that embed errors which may quote a credential-bearing URL.
///
/// The userinfo is also redacted wherever else it appears, because tools
/// such as `git` quote the URL in a rewritten form.
pub fn redact_url_credentials(text: &str, url: &str) -> String {
    let redacted = text.replace(url, &sanitize_url(url));
    let userinfo = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .map(|(userinfo, _)| userinfo)
        .filter(|userinfo| !userinfo.is_empty() && !userinfo.contains('/'));
    match userinfo {
        Some(userinfo) => redacted.replace(&format!("{userinfo}@"), "***@"),
        None => redacted,
    }
}
