//! Embeds `oauthrelay-core` to serve the OAuth extension's `/oidc/{project}/{extension}/*` endpoints.

#![allow(dead_code)]

pub mod http;
pub mod layout;
pub mod rate_limit;
pub mod resolver;
pub mod sealer;
pub mod store;
