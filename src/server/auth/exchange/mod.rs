//! RFC 8693 token-exchange endpoint.
//!
//! The inbound analogue of `workload_tokens/`: a caller presents an external
//! OIDC subject token (the credential) plus an optional Rise project and
//! receives a short-lived, Rise-signed access token encoding the resolved
//! principal. It is a public route — the subject token is the credential.

pub mod handlers;
pub mod models;
pub mod routes;
