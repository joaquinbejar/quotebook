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

/// Minimum length of a URL path segment or query value treated as a secret token.
const MIN_TOKEN_LEN: usize = 8;

/// Scrubs one RPC URL from messages: the URL itself (as [`redact_urls`] does for every URL) and
/// every path segment, query value and userinfo part of 8 or more characters
/// as a bare token, because providers echo the key alone (`/?api-key=KEY`, QuickNode's
/// `/TOKEN/`).
#[derive(Clone, Default)]
pub struct Redactor {
    tokens: Vec<String>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("tokens", &self.tokens.len())
            .finish()
    }
}

impl Redactor {
    /// The tokens of `url`.
    #[must_use]
    pub fn new(url: &str) -> Self {
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        let mut parts: Vec<&str> = Vec::new();
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            parts.extend(userinfo.split(':'));
        }
        let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
        let query = query.split('#').next().unwrap_or("");
        parts.extend(path.split('/'));
        parts.extend(
            query
                .split('&')
                .map(|kv| kv.split_once('=').map_or(kv, |(_, v)| v)),
        );
        let mut tokens: Vec<String> = parts
            .into_iter()
            .filter(|t| t.len() >= MIN_TOKEN_LEN)
            .map(str::to_owned)
            .collect();
        // Longest first, so a token that contains another is replaced whole.
        tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
        tokens.dedup();
        Self { tokens }
    }

    /// `message` with every URL and every token of this redactor's URL replaced.
    #[must_use]
    pub fn redact(&self, message: &str) -> String {
        let mut out = redact_urls(message);
        for token in &self.tokens {
            out = out.replace(token.as_str(), "<redacted>");
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::{redact_urls, Redactor};

    #[test]
    fn test_redactor_scrubs_bare_tokens() {
        let r = Redactor::new("https://user:hunter2-password@rpc.example.com/QUICKNODE-TOKEN-1/?api-key=SECRET-KEY-42&cluster=devnet");
        let m = r.redact(
            "failed: https://rpc.example.com/QUICKNODE-TOKEN-1/?api-key=SECRET-KEY-42; key SECRET-KEY-42, path QUICKNODE-TOKEN-1, pass hunter2-password",
        );
        for secret in [
            "SECRET-KEY-42",
            "QUICKNODE-TOKEN-1",
            "hunter2-password",
            "rpc.example.com",
        ] {
            assert!(!m.contains(secret), "{m}");
        }
        // Short values are not treated as secrets.
        assert!(r.redact("devnet").contains("devnet"));
    }

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
