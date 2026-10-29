use axum::http::{header, HeaderMap};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::{
    cookie_helpers::CookieSettings,
    token_storage::{generate_code_challenge, generate_state_token},
};

/// Browser-held proof shared by sign-in transactions on one host and stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserProof {
    cookie_name: String,
    digest: String,
}

impl BrowserProof {
    pub fn new(stage: &str, settings: &CookieSettings, headers: &HeaderMap) -> (Self, String) {
        let prefix = if settings.secure { "__Host-" } else { "" };
        let mut proof = Self {
            cookie_name: format!("{prefix}rise_oauth_{stage}"),
            digest: String::new(),
        };
        let secret = proof
            .secret(headers)
            .map(str::to_owned)
            .unwrap_or_else(generate_state_token);
        proof.digest = generate_code_challenge(&secret);
        let cookie = proof.cookie(&secret, 600, settings);
        (proof, cookie)
    }

    fn secret<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        let mut values = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .filter(|(name, _)| *name == self.cookie_name)
            .map(|(_, value)| value);
        let secret = values.next()?;
        if values.next().is_some()
            || secret.len() != 43
            || !secret
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return None;
        }
        Some(secret)
    }

    pub fn matches(&self, headers: &HeaderMap) -> bool {
        let Some(secret) = self.secret(headers) else {
            return false;
        };
        generate_code_challenge(secret)
            .as_bytes()
            .ct_eq(self.digest.as_bytes())
            .into()
    }

    fn cookie(&self, secret: &str, max_age: u64, settings: &CookieSettings) -> String {
        let secure = if settings.secure { "; Secure" } else { "" };
        format!(
            "{}={secret}; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Lax{secure}",
            self.cookie_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie_headers(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().unwrap().parse().unwrap(),
        );
        headers
    }

    #[test]
    fn browser_proofs_are_host_only_secure_and_cannot_be_transferred_between_cookie_jars() {
        let settings = CookieSettings {
            secure: true,
            cookie_domain: Some(".example.com".into()),
        };
        let (first, first_cookie) = BrowserProof::new("app", &settings, &HeaderMap::new());
        let (second, second_cookie) = BrowserProof::new("app", &settings, &HeaderMap::new());
        assert!(first_cookie.starts_with("__Host-"));
        assert!(first_cookie.contains("; Secure"));
        assert!(first_cookie.contains("; HttpOnly"));
        assert!(first_cookie.contains("; SameSite=Lax"));
        assert!(!first_cookie.contains("Domain="));
        assert!(first.matches(&cookie_headers(&first_cookie)));
        assert!(second.matches(&cookie_headers(&second_cookie)));
        assert!(!first.matches(&cookie_headers(&second_cookie)));
        assert!(!second.matches(&cookie_headers(&first_cookie)));
        assert!(!first.matches(&HeaderMap::new()));
        assert!(first_cookie.contains("Max-Age=600"));
    }

    #[test]
    fn starts_reuse_one_nonce_per_stage_and_reject_malformed_or_duplicate_cookies() {
        let settings = CookieSettings {
            secure: false,
            cookie_domain: None,
        };
        let (first, cookie) = BrowserProof::new("central", &settings, &HeaderMap::new());
        let mut headers = cookie_headers(&cookie);
        let (second, refreshed) = BrowserProof::new("central", &settings, &headers);
        assert_eq!(cookie, refreshed);
        assert!(first.matches(&headers));
        assert!(second.matches(&headers));
        let (app, app_cookie) = BrowserProof::new("app", &settings, &headers);
        assert!(!app.matches(&headers));
        headers.append(
            header::COOKIE,
            app_cookie.split(';').next().unwrap().parse().unwrap(),
        );
        assert!(app.matches(&headers));
        assert!(first.matches(&headers));
        headers.append(
            header::COOKIE,
            cookie.split(';').next().unwrap().parse().unwrap(),
        );
        assert!(!first.matches(&headers));
        let (_, replacement) = BrowserProof::new("central", &settings, &headers);
        assert_ne!(cookie, replacement);
        for malformed in ["short".to_string(), "!".repeat(43)] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::COOKIE,
                format!("rise_oauth_central={malformed}").parse().unwrap(),
            );
            assert!(!first.matches(&headers));
            let (_, replacement) = BrowserProof::new("central", &settings, &headers);
            assert!(!replacement.starts_with(&format!("rise_oauth_central={malformed};")));
        }
    }
}
