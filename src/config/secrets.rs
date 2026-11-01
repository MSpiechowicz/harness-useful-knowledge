//! Keeping the database URL and token out of messages: [`redact_url`] shows where a URL points
//! without its password, and [`Secrets`] masks every secret part of a URL and a token in free text.

use crate::support::redact::redact_text;

/// What the mask of a secret looks like.
const MASK: &str = "••••";

/// `url` without its password and query: `scheme://user@host:port/path`, with `?…` when the URL
/// had a query (which can hold a token). Only the part after the last `@` is shown as the host,
/// so a password with an unescaped `@`, `/` or `?` in it is never shown.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return MASK.to_owned();
    };

    let (user, after) = match rest.rfind('@') {
        Some(at) => {
            let userinfo = &rest[..at];
            let user = userinfo.split(':').next().unwrap_or_default();
            (Some(user), &rest[at + 1..])
        }
        None => (None, rest),
    };

    let end = after.find(['?', '#']).unwrap_or(after.len());
    let location = &after[..end];
    let query = if end < after.len() { "?…" } else { "" };

    match user.filter(|user| !user.is_empty()) {
        Some(user) => format!("{scheme}://{user}@{location}{query}"),
        None => format!("{scheme}://{location}{query}"),
    }
}

/// The secret parts of a URL and a token, masked out of messages by [`Secrets::scrub`].
#[derive(Default, Clone, PartialEq, Eq)]
pub struct Secrets(Vec<String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secrets({})", self.0.len())
    }
}

impl Secrets {
    /// The whole URL, its password (as written and decoded), the values of query parameters that
    /// look like credentials, and the token.
    pub fn of(url: &str, token: Option<&str>) -> Self {
        let mut secrets: Vec<String> = vec![url.trim().to_owned()];

        if let Some((_, rest)) = url.split_once("://")
            && let Some(at) = rest.rfind('@')
            && let Some((_, password)) = rest[..at].split_once(':')
        {
            secrets.push(password.to_owned());
            secrets.push(percent_decode_lossy(password));
        }

        if let Some((_, query)) = url.split_once('?') {
            for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
                let key = key.to_ascii_lowercase();
                let sensitive = ["password", "token", "secret", "key", "auth"]
                    .iter()
                    .any(|word| key.contains(word));

                if sensitive {
                    secrets.push(value.to_owned());
                    secrets.push(percent_decode_lossy(value));
                }
            }
        }

        secrets.extend(token.map(str::to_owned));
        secrets.retain(|secret| !secret.is_empty());
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self(secrets)
    }

    /// `message` with every secret replaced by `••••`, then through the general secret filter.
    pub fn scrub(&self, message: &str) -> String {
        let mut out = message.to_owned();

        for secret in &self.0 {
            out = out.replace(secret.as_str(), MASK);
        }

        redact_text(&out)
    }
}

/// Decodes `%XX` escapes, keeping anything malformed as written.
fn percent_decode_lossy(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| raw.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());

        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_urls_keep_the_place_and_drop_the_secrets() {
        assert_eq!(
            redact_url("postgres://alice:hunter2@db.example.com:5432/knowledge?sslmode=require"),
            "postgres://alice@db.example.com:5432/knowledge?…"
        );
        assert_eq!(
            redact_url("mongodb://u:pw@db.example.com/m?authSource=admin"),
            "mongodb://u@db.example.com/m?…"
        );
        assert_eq!(redact_url("postgres://u:p@ss/w?rd@db/m"), "postgres://u@db/m");
        assert_eq!(redact_url("not a url"), MASK);
    }

    #[test]
    fn scrubbing_masks_the_password_token_and_url() {
        let secrets = Secrets::of("postgres://u:p%40ss@db/m?password=other", Some("tok-123"));
        let message = "failed for postgres://u:p%40ss@db/m?password=other p@ss, other, tok-123";
        let scrubbed = secrets.scrub(message);

        for secret in ["p%40ss", "p@ss", "other", "tok-123"] {
            assert!(!scrubbed.contains(secret), "{scrubbed}");
        }
    }

    #[test]
    fn the_debug_form_of_secrets_shows_a_count_only() {
        let shown = format!("{:?}", Secrets::of("postgres://u:hunter2@db/m", Some("tok-123")));

        assert!(!shown.contains("hunter2") && !shown.contains("tok-123"), "{shown}");
    }
}
