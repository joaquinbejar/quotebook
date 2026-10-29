//! The CLI error type.

use quotebook_maker::client::ClientError;

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A command-line argument is missing or invalid.
    #[error("argument: {0}")]
    Args(String),
    /// A keypair file could not be read. Carries the path, never the contents.
    #[error("cannot read keypair file {0}")]
    Keypair(String),
    /// The RPC call failed or the transaction was rejected.
    #[error("rpc: {0}")]
    Rpc(String),
    /// An account is missing or is not what it should be.
    #[error("account: {0}")]
    Account(String),
    /// The maker refused, or its quote fails a check.
    #[error("quote: {0}")]
    Quote(#[from] ClientError),
    /// The maker service could not be configured.
    #[error("maker: {0}")]
    Maker(String),
}

/// Replaces every URL in `message` (anything from `http://`, `https://`, `ws://` or `wss://` to
/// the next whitespace, quote or closing bracket) with `<redacted-url>`. RPC provider URLs often
/// embed API keys, and HTTP client errors quote the request URL.
#[must_use]
pub fn redact_urls(message: &str) -> String {
    const SCHEMES: [&str; 4] = ["https://", "http://", "wss://", "ws://"];
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    loop {
        let next = SCHEMES.iter().filter_map(|s| rest.find(s)).min();
        let Some(start) = next else {
            out.push_str(rest);
            return out;
        };
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        out.push_str("<redacted-url>");
        let end = from
            .find(|c: char| {
                c.is_whitespace() || matches!(c, ')' | ']' | '}' | '"' | '\'' | '>' | ',')
            })
            .unwrap_or(from.len());
        rest = from.get(end..).unwrap_or("");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::redact_urls;

    #[test]
    fn test_redact_urls() {
        assert_eq!(
            redact_urls(
                "error sending request for url (https://rpc.example.com/?api-key=SECRET): refused"
            ),
            "error sending request for url (<redacted-url>): refused"
        );
        assert_eq!(
            redact_urls("a http://k@h:1/p b wss://x/y?t=1, c"),
            "a <redacted-url> b <redacted-url>, c"
        );
        assert_eq!(redact_urls("no url here"), "no url here");
        assert_eq!(redact_urls("https://end"), "<redacted-url>");
    }
}
