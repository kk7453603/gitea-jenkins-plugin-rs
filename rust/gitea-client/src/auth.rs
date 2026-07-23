//! Authentication strategies for the Gitea HTTP client.
//!
//! Maps 1:1 onto the upstream Java types
//! (`GiteaAuthNone`, `GiteaAuthToken`, `GiteaAuthUser`).
//!
//! Gitea's token scheme uses the non-standard
//! `Authorization: token <T>` header (NOT `Bearer`), matching the
//! upstream `DefaultGiteaConnection.withAuthentication(...)`.

use base64::Engine;
use reqwest::RequestBuilder;

/// Authentication strategy.
#[derive(Debug, Clone)]
pub enum Auth {
    /// Anonymous access. Sends no `Authorization` header.
    None,
    /// Gitea personal access token. Header: `Authorization: token <T>`.
    Token(String),
    /// HTTP Basic auth. Header: `Authorization: Basic <base64(user:pass)>`.
    Basic {
        /// Username.
        user: String,
        /// Password / token.
        pass: String,
    },
}

impl Auth {
    /// Apply this authentication strategy to a `reqwest::RequestBuilder`.
    ///
    /// For [`Auth::None`] this is a no-op.
    pub fn apply(&self, req_builder: RequestBuilder) -> RequestBuilder {
        match self {
            Auth::None => req_builder,
            Auth::Token(token) => {
                // Gitea-specific scheme: `token <T>`, not `Bearer <T>`.
                req_builder.header("Authorization", format!("token {}", token))
            }
            Auth::Basic { user, pass } => {
                let raw = format!("{}:{}", user, pass);
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                req_builder.header("Authorization", format!("Basic {}", encoded))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_value_preserved() {
        match Auth::Token("abc".to_string()) {
            Auth::Token(t) => assert_eq!(t, "abc"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn basic_value_preserved() {
        let a = Auth::Basic {
            user: "u".to_string(),
            pass: "p".to_string(),
        };
        match a {
            Auth::Basic { user, pass } => {
                assert_eq!(user, "u");
                assert_eq!(pass, "p");
            }
            _ => panic!("wrong variant"),
        }
    }
}
