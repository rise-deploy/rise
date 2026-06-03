//! Re-exports of the Rise token signer and claim types.
//!
//! The token-signing/verification core now lives in the pure `rise-backend-auth`
//! crate. This module preserves the historical `crate::server::auth::jwt_signer`
//! paths by re-exporting the relevant types under their previous names.
//!
//! - [`JwtSigner`] is the crate's `RiseTokenSigner` (same `new` / `generate_jwks`
//!   / `sign_workload_jwt` signatures; `sign_user_jwt` / `sign_ingress_jwt` now
//!   take `groups: Option<Vec<String>>` and are synchronous; the verify adapters
//!   `verify_user_jwt` / `verify_jwt_skip_aud` are preserved).
pub use rise_backend_auth::{RiseTokenSigner as JwtSigner, WorkloadSubjectInfo};
