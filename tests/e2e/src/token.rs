//! Token helpers: mint the HS256 CI bearer (reusing the server's signer) the
//! same way the backend signs sessions.

use anyhow::Result;

/// Mint the admin CI bearer the same way the backend signs sessions: HS256 over
/// the shared secret, `iss = public_url`, `email = admin@example.com` (an admin
/// user, so it bypasses ownership checks). Mirrors the bash `create_rise_ci_token`.
pub fn mint_ci_token(secret_b64: &str, public_url: &str) -> Result<String> {
    let signer = rise_backend_auth::RiseTokenSigner::new(
        secret_b64,
        public_url.to_string(),
        3600,
        vec!["sub".to_string(), "email".to_string(), "name".to_string()],
        None,
        None,
    )
    .map_err(|e| anyhow::anyhow!("build CI token signer: {e}"))?;
    let claims = serde_json::json!({
        "sub": "rise-ci",
        "email": "admin@example.com",
        "name": "Rise CI",
    });
    signer
        .sign_user_jwt(&claims, None, public_url, None)
        .map_err(|e| anyhow::anyhow!("sign CI token: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    // 32 zero-bytes, base64 — the local/e2e Docker signing secret.
    const SECRET_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    #[test]
    fn mints_admin_bearer_with_expected_claims() {
        let url = "http://rise.localhost:3000";
        let tok = mint_ci_token(SECRET_B64, url).expect("mint");

        // Verify the token via the signer's own HS256/aud-checked path, so the
        // assertions cover a signature-valid token (not just a decoded payload).
        let signer = rise_backend_auth::RiseTokenSigner::new(
            SECRET_B64,
            url.to_string(),
            3600,
            vec!["sub".to_string(), "email".to_string(), "name".to_string()],
            None,
            None,
        )
        .expect("signer");
        let claims = signer.verify_user_jwt(&tok, url).expect("verify");
        assert_eq!(claims.email, "admin@example.com");
        assert_eq!(claims.iss, url);

        // HS256 header.
        let header = tok.split('.').next().unwrap();
        let hdr: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(header)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(hdr["alg"], "HS256");
    }
}
